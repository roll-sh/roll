//! The STS query API on the S3 listener, so SDKs pointed at it (`endpoint_url` for `sts`) get
//! temporary credentials: `POST /` (or `GET /?Action=…`) with `Action=…&Version=2011-06-15`,
//! form-encoded, answered in STS's XML. The IAM query API shares the endpoint (`iam/`).
//!
//! - `GetSessionToken`: a session of the calling user (or root, or account key), optionally
//!   with MFA (`SerialNumber`, `TokenCode`).
//! - `AssumeRole`: a role's session, for a caller its trust allows: an IAM trust policy
//!   (`AssumeRolePolicyDocument`: accounts, users, roles, conditions such as `sts:ExternalId`
//!   and `aws:MultiFactorAuthPresent`), or a control-plane role's trust (roll keys and an external
//!   id). Another account's role needs the caller's own policies to allow `sts:AssumeRole` too.
//!   Role chaining (a role session assuming a role) caps the session at an hour, as on AWS.
//!   Session policies inline (`Policy`) and managed (`PolicyArns`).
//! - `AssumeRoleWithWebIdentity`: a role's, for an OIDC ID token from a provider its trust names
//!   (unsigned: the token is the proof). See [`oidc`].
//! - `GetFederationToken`: a federated user's session, the calling user's permissions bounded by
//!   the session policies.
//! - `GetCallerIdentity`.
//!
//! Signed calls use SigV4 with service `sts` (any region). Errors are STS's
//! (`<ErrorResponse><Error><Type>…<Code>…`).

pub mod oidc;

use crate::ctx::Ctx;
use crate::identity::{Identity, Kind};
use crate::policy::{Context, Policy, Principal, Requester, Via};
use crate::temporary;
use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use s3_proto::xml::escape;
use s3_proto::{Code, S3Error};
use s3_store::sts::{self as st, OidcProvider, Role, Session, SessionRole, WebIdentity};
use s3_store::{AccessKey, Permission, Store};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const XMLNS: &str = "https://sts.amazonaws.com/doc/2011-06-15/";
const VERSION: &str = "2011-06-15";
/// Largest request body: a 20 000-character web identity token, a 2 KB policy and tags fit
/// easily.
pub(crate) const MAX_BODY: usize = 64 * 1024;
/// A role session assuming a role (role chaining) gets at most an hour, as on AWS.
const CHAINED_MAX_S: i64 = 3600;
/// The root user's GetSessionToken and GetFederationToken sessions: at most an hour.
const ROOT_MAX_S: i64 = 3600;
/// Managed session policies per request.
const MAX_POLICY_ARNS: usize = 10;

/// Whether a request is for STS (or IAM) rather than S3: the service endpoint (`/`, no bucket)
/// with a form body or an `Action` parameter, or a CORS preflight of a `POST` there (S3 has no
/// `POST /`; other preflights of `/` stay S3's).
pub(crate) fn is_sts(ctx: &Ctx) -> bool {
    if ctx.bucket.is_some() || ctx.key.is_some() || ctx.website {
        return false;
    }
    match ctx.method {
        Method::POST => ctx.h("content-type").is_some_and(|c| c.trim().to_ascii_lowercase().starts_with("application/x-www-form-urlencoded")) || ctx.has("Action"),
        Method::GET => ctx.has("Action"),
        Method::OPTIONS => ctx.h("access-control-request-method").is_some_and(|m| m.trim().eq_ignore_ascii_case("POST")),
        _ => false,
    }
}

/// An STS (or IAM) error.
#[derive(Debug)]
pub(crate) struct StsError {
    pub code: &'static str,
    pub status: u16,
    pub message: String,
}

impl StsError {
    pub(crate) fn new(code: &'static str, status: u16, message: impl Into<String>) -> Self {
        Self { code, status, message: message.into() }
    }
}

pub(crate) fn access_denied(m: impl Into<String>) -> StsError {
    StsError::new("AccessDenied", 403, m)
}

pub(crate) fn validation(m: impl Into<String>) -> StsError {
    StsError::new("ValidationError", 400, m)
}

fn malformed_policy(m: impl Into<String>) -> StsError {
    StsError::new("MalformedPolicyDocument", 400, m)
}

/// The STS error for a failed signature check (S3's codes in, STS's out).
pub(crate) fn auth_error(e: S3Error) -> StsError {
    match e.code {
        Code::InvalidAccessKeyId | Code::InvalidToken => StsError::new("InvalidClientTokenId", 403, "The security token included in the request is invalid."),
        Code::ExpiredToken => StsError::new("ExpiredToken", 403, "The security token included in the request is expired"),
        Code::SignatureDoesNotMatch => StsError::new("SignatureDoesNotMatch", 403, "The request signature we calculated does not match the signature you provided. Check your AWS Secret Access Key and signing method. Consult the service documentation for details."),
        Code::RequestTimeTooSkewed => StsError::new("SignatureDoesNotMatch", 403, "Signature expired: the request time is too far from the server's time."),
        Code::AccessDenied => access_denied(e.message),
        Code::AuthorizationHeaderMalformed | Code::AuthorizationQueryParametersError | Code::InvalidRequest | Code::InvalidArgument => StsError::new("IncompleteSignature", 400, e.message),
        Code::ServiceUnavailable => StsError::new("ServiceUnavailable", 503, "Service is unable to handle request."),
        _ => StsError::new("InternalFailure", 500, "The request processing has failed because of an unknown error, exception or failure."),
    }
}

pub(crate) fn store_failure(e: s3_store::StoreError) -> StsError {
    match e {
        s3_store::StoreError::InvalidArgument(m) => validation(m),
        s3_store::StoreError::Unavailable(_) => StsError::new("ServiceUnavailable", 503, "Service is unable to handle request."),
        e => {
            tracing::error!(error = %e, "sts: storage error");
            StsError::new("InternalFailure", 500, "The request processing has failed because of an unknown error, exception or failure.")
        }
    }
}

/// Answer an STS request (errors included: they're STS's XML, not S3's).
pub(crate) async fn handle(ctx: &mut Ctx, body: Body) -> Response {
    let request_id = request_id();
    let resp = if ctx.method == Method::OPTIONS {
        preflight(ctx)
    } else {
        match run(ctx, body, &request_id).await {
            Ok(xml) => xml_response(StatusCode::OK, xml),
            Err(e) => error_response(XMLNS, &e, &request_id),
        }
    };
    decorate(ctx, resp, &request_id)
}

/// Browsers may call STS directly (web identity); nothing here relies on cookies.
pub(crate) fn decorate(ctx: &Ctx, mut resp: Response, request_id: &str) -> Response {
    if ctx.h("origin").is_some() {
        crate::response::set(&mut resp, "access-control-allow-origin", "*");
        crate::response::set(&mut resp, "access-control-expose-headers", "x-amzn-requestid, x-amz-request-id");
    }
    crate::response::set(&mut resp, "x-amzn-requestid", request_id);
    resp
}

pub(crate) fn error_response(xmlns: &str, e: &StsError, request_id: &str) -> Response {
    if e.status >= 500 {
        tracing::warn!(code = e.code, "sts/iam request failed");
    }
    let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_REQUEST);
    let kind = if e.status >= 500 { "Receiver" } else { "Sender" };
    xml_response(
        status,
        format!("<ErrorResponse xmlns=\"{xmlns}\"><Error><Type>{kind}</Type><Code>{}</Code><Message>{}</Message></Error><RequestId>{request_id}</RequestId></ErrorResponse>", e.code, escape(&e.message)),
    )
}

pub(crate) fn request_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("os randomness");
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

pub(crate) fn xml_response(status: StatusCode, body: String) -> Response {
    let mut r = Response::new(Body::from(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{body}")));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml"));
    r
}

fn preflight(ctx: &Ctx) -> Response {
    let mut r = Response::new(Body::empty());
    crate::response::set(&mut r, "access-control-allow-methods", "GET, POST");
    if let Some(h) = ctx.h("access-control-request-headers") {
        let h: String = h.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | ',' | ' ' | '_')).take(2048).collect();
        crate::response::set(&mut r, "access-control-allow-headers", &h);
    }
    crate::response::set(&mut r, "access-control-max-age", "3600");
    r
}

/// The request's parameters: the query string's, then the form body's.
pub(crate) fn form(raw: &str) -> Vec<(String, String)> {
    // Form encoding writes spaces as `+`.
    s3_proto::http::parse_query(&raw.replace('+', "%20"))
}

/// A query API request's parameters.
pub(crate) struct Params(pub Vec<(String, String)>);

impl Params {
    /// Read the body (at most [`MAX_BODY`]) and the query string.
    pub(crate) async fn read(ctx: &Ctx, body: Body) -> Result<(Self, bytes::Bytes), StsError> {
        let body = axum::body::to_bytes(body, MAX_BODY).await.map_err(|_| validation(format!("The request body is larger than {MAX_BODY} bytes.")))?;
        let text = std::str::from_utf8(&body).map_err(|_| validation("The request body isn't UTF-8."))?;
        let mut params = form(&ctx.raw_query);
        params.extend(form(text));
        Ok((Self(params), body))
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub(crate) fn required(&self, name: &str, field: &str) -> Result<&str, StsError> {
        self.get(name).ok_or_else(|| validation(format!("1 validation error detected: Value null at '{field}' failed to satisfy constraint: Member must not be null")))
    }

    /// `Prefix.member.N.<suffix>` values in member order (`suffix` empty for plain lists).
    pub(crate) fn members(&self, prefix: &str, suffix: &str) -> Result<Vec<(u32, String)>, StsError> {
        let mut out = BTreeMap::new();
        for (k, v) in &self.0 {
            let Some(rest) = k.strip_prefix(prefix).and_then(|r| r.strip_prefix(".member.")) else { continue };
            let (n, s) = rest.split_once('.').map_or((rest, ""), |(n, s)| (n, s));
            if s != suffix {
                continue;
            }
            let n: u32 = n.parse().map_err(|_| validation(format!("invalid parameter {k}")))?;
            if n == 0 || n > 1000 {
                return Err(validation(format!("invalid parameter {k}")));
            }
            out.insert(n, v.clone());
        }
        Ok(out.into_iter().collect())
    }

    /// `Prefix.member.N` as a plain list.
    pub(crate) fn list(&self, prefix: &str) -> Result<Vec<String>, StsError> {
        Ok(self.members(prefix, "")?.into_iter().map(|(_, v)| v).collect())
    }

    /// `Tags.member.N.Key/Value`.
    pub(crate) fn tags(&self, prefix: &str) -> Result<Vec<(String, String)>, StsError> {
        let keys = self.members(prefix, "Key")?;
        let values = self.members(prefix, "Value")?;
        Ok(keys.into_iter().map(|(n, k)| (k, values.iter().find(|(m, _)| *m == n).map(|(_, v)| v.clone()).unwrap_or_default())).collect())
    }

    fn duration(&self, default: i64, max: i64, why: &str) -> Result<i64, StsError> {
        let Some(v) = self.get("DurationSeconds") else { return Ok(default.min(max)) };
        let d: i64 = v.trim().parse().map_err(|_| validation(format!("1 validation error detected: Value '{v}' at 'durationSeconds' failed to satisfy constraint: Member must be a number")))?;
        if d < st::MIN_SESSION_S {
            return Err(validation(format!("1 validation error detected: Value '{d}' at 'durationSeconds' failed to satisfy constraint: Member must have value greater than or equal to {}", st::MIN_SESSION_S)));
        }
        if d > max {
            return Err(validation(if why.is_empty() { format!("1 validation error detected: Value '{d}' at 'durationSeconds' failed to satisfy constraint: Member must have value less than or equal to {max}") } else { why.to_string() }));
        }
        Ok(d)
    }

    /// `Policy` (checked and minified).
    fn session_policy(&self) -> Result<Option<String>, StsError> {
        let Some(p) = self.get("Policy") else { return Ok(None) };
        if p.is_empty() || p.len() > st::MAX_SESSION_POLICY_BYTES {
            return Err(validation(format!("1 validation error detected: Value at 'policy' failed to satisfy constraint: Member must have length less than or equal to {}", st::MAX_SESSION_POLICY_BYTES)));
        }
        crate::validate_identity_policy(p, st::MAX_SESSION_POLICY_BYTES).map(Some).map_err(malformed_policy)
    }

    /// `PolicyArns.member.N.arn`: managed policies of `account` (or AWS's) that exist.
    fn policy_arns(&self, store: &Store, account: &str) -> Result<Vec<String>, StsError> {
        let arns: Vec<String> = self.members("PolicyArns", "arn")?.into_iter().map(|(_, v)| v).collect();
        if arns.len() > MAX_POLICY_ARNS {
            return Err(validation(format!("1 validation error detected: Value at 'policyArns' failed to satisfy constraint: Member must have length less than or equal to {MAX_POLICY_ARNS}")));
        }
        for arn in &arns {
            if crate::identity::managed_document(store, account, arn).is_none() {
                return Err(StsError::new("InvalidParameterValue", 400, format!("Policy {arn} does not exist or is not attachable.")));
            }
        }
        Ok(arns)
    }

    /// `SerialNumber` and `TokenCode`, both or neither.
    fn mfa(&self) -> Result<Option<(&str, &str)>, StsError> {
        match (self.get("SerialNumber"), self.get("TokenCode")) {
            (None, None) => Ok(None),
            (Some(s), Some(c)) => {
                if s.len() < 9 || s.len() > 256 {
                    return Err(validation("1 validation error detected: Value at 'serialNumber' failed to satisfy constraint: Member must have length between 9 and 256"));
                }
                if crate::mfa::parse_code(c).is_none() {
                    return Err(validation("1 validation error detected: Value at 'tokenCode' failed to satisfy constraint: Member must satisfy regular expression pattern: [\\d]*"));
                }
                Ok(Some((s, c)))
            }
            _ => Err(validation("SerialNumber and TokenCode go together")),
        }
    }
}

async fn run(ctx: &mut Ctx, body: Body, request_id: &str) -> Result<String, StsError> {
    let (p, body) = Params::read(ctx, body).await?;
    let action = p.get("Action").ok_or_else(|| StsError::new("MissingAction", 400, "Missing Action"))?.to_string();
    if let Some(v) = p.get("Version")
        && v != VERSION
    {
        return Err(StsError::new("InvalidAction", 400, format!("Could not find operation {action} for version {v}")));
    }
    let store = ctx.store().clone();
    let result = match action.as_str() {
        "AssumeRoleWithWebIdentity" => assume_role_with_web_identity(ctx, &store, &p).await?,
        "GetCallerIdentity" | "GetSessionToken" | "AssumeRole" | "GetFederationToken" => {
            let caller = authenticate(ctx, &body, "sts")?;
            let id = crate::identity::resolve(&store, &caller).map_err(store_failure)?;
            match action.as_str() {
                "GetCallerIdentity" => caller_identity(&id),
                "GetSessionToken" => get_session_token(ctx, &store, &caller, &id, &p).await?,
                "GetFederationToken" => get_federation_token(ctx, &store, &caller, &id, &p).await?,
                _ => assume_role(ctx, &store, &caller, &id, &p).await?,
            }
        }
        "AssumeRoleWithSAML" | "DecodeAuthorizationMessage" | "GetAccessKeyInfo" | "AssumeRoot" | "GetDelegatedAccessToken" => {
            return Err(StsError::new("InvalidAction", 400, format!("{action} isn't supported by roll (see docs/storage/compatibility.md)")));
        }
        _ => return Err(StsError::new("InvalidAction", 400, format!("Could not find operation {action} for version {VERSION}"))),
    };
    Ok(format!(
        "<{action}Response xmlns=\"{XMLNS}\"><{action}Result>{result}</{action}Result><ResponseMetadata><RequestId>{request_id}</RequestId></ResponseMetadata></{action}Response>"
    ))
}

/// Verify a signed query API call (SigV4, service `service`). The payload is the body.
pub(crate) fn authenticate(ctx: &Ctx, body: &[u8], service: &str) -> Result<AccessKey, StsError> {
    let pairs = ctx.header_pairs();
    let headers: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let req = s3_proto::sigv4::Request { method: ctx.method.as_str(), path: &ctx.raw_path, query: &ctx.raw_query, headers: &headers };
    let hash = hex::encode(Sha256::digest(body));
    let token = temporary::request_token(&ctx.headers, &ctx.query);
    let mut found = None;
    let mut failure = None;
    let r = s3_proto::sigv4::authenticate_service(&req, ctx.now_s(), service, &hash, |id| match temporary::resolve(ctx.store(), id, token) {
        Ok(k) => {
            let s = k.secret.clone();
            found = Some(k);
            Some(s)
        }
        Err(e) => {
            failure = Some(e);
            None
        }
    });
    match r {
        Ok(Some(_)) => {
            let k = found.ok_or_else(|| StsError::new("InternalFailure", 500, "no key"))?;
            if let Some(id) = k.acting_key() {
                ctx.store().touch_key(id);
            }
            Ok(k)
        }
        Ok(None) => Err(StsError::new("MissingAuthenticationToken", 403, "Request is missing Authentication Token")),
        Err(e) => Err(auth_error(match failure {
            Some(f) if e.code == Code::InvalidAccessKeyId => f,
            _ => e,
        })),
    }
}

fn caller_identity(id: &Identity) -> String {
    format!("<Arn>{}</Arn><UserId>{}</UserId><Account>{}</Account>", escape(&id.caller_arn), escape(&id.user_id), id.account.id)
}

/// `YYYY-MM-DDTHH:MM:SSZ`.
fn expiration(ms: i64) -> String {
    let s = s3_proto::time::iso8601_ms(ms.div_euclid(1000) * 1000);
    s.strip_suffix(".000Z").map_or(s.clone(), |b| format!("{b}Z"))
}

fn credentials_xml(c: &st::Credentials) -> String {
    format!(
        "<Credentials><AccessKeyId>{}</AccessKeyId><SecretAccessKey>{}</SecretAccessKey><SessionToken>{}</SessionToken><Expiration>{}</Expiration></Credentials>",
        c.access_key_id,
        escape(&c.secret_access_key),
        escape(&c.session_token),
        expiration(c.expiration)
    )
}

/// Check the caller's MFA code: the device must be its own (the root's, or its user's).
async fn caller_mfa(store: &Store, id: &Identity, serial: &str, code: &str) -> Result<i64, StsError> {
    let owner = match &id.kind {
        Kind::Root => None,
        Kind::User { name, .. } => Some(name.as_str()),
        _ => return Err(access_denied("MultiFactorAuthentication failed: only the root user and IAM users have MFA devices.")),
    };
    if crate::mfa::check(store, &id.account.id, serial, code, owner).await.map_err(store_failure)? {
        Ok(store.now())
    } else {
        Err(access_denied("MultiFactorAuthentication failed with invalid MFA one time pass code. "))
    }
}

async fn get_session_token(_ctx: &Ctx, store: &Store, k: &AccessKey, id: &Identity, p: &Params) -> Result<String, StsError> {
    if k.is_temporary() {
        return Err(access_denied("Cannot call GetSessionToken with session credentials"));
    }
    let (default, max) = if id.is_root() { (ROOT_MAX_S, ROOT_MAX_S) } else { (st::GET_SESSION_TOKEN_DEFAULT_S, st::GET_SESSION_TOKEN_MAX_S) };
    let d = p.duration(default, max, "")?;
    let mfa_at = match p.mfa()? {
        Some((serial, code)) => Some(caller_mfa(store, id, serial, code).await?),
        None => None,
    };
    let mut s = Session::new(k.permission);
    s.key = Some(k.id.clone());
    s.buckets = k.buckets.clone();
    s.account = k.account.clone();
    s.user = k.user.clone();
    s.mfa_at = mfa_at;
    let (_, c) = store.mint_session(s, d).await.map_err(store_failure)?;
    Ok(credentials_xml(&c))
}

/// `GetFederationToken`'s `Name`: 2-32 characters of letters, digits and `_+=,.@-`.
fn validate_federated_name(n: &str) -> Result<(), StsError> {
    if n.len() < 2 || n.len() > 32 || !n.chars().all(|c| c.is_ascii_alphanumeric() || "_+=,.@-".contains(c)) {
        return Err(validation("1 validation error detected: Value at 'name' failed to satisfy constraint: Member must have length between 2 and 32 and satisfy regular expression pattern: [\\w+=,.@-]*"));
    }
    Ok(())
}

async fn get_federation_token(ctx: &Ctx, store: &Store, k: &AccessKey, id: &Identity, p: &Params) -> Result<String, StsError> {
    if k.is_temporary() {
        return Err(access_denied("Cannot call GetFederationToken with session credentials"));
    }
    let name = p.required("Name", "name")?;
    validate_federated_name(name)?;
    let acct = id.account.id.clone();
    let cond = StsCond { ctx, id: Some(id), p, extra: Vec::new() };
    if !id.verdict("sts:GetFederationToken", &format!("arn:aws:sts::{acct}:federated-user/{name}"), &cond, id.kind == Kind::Key).allowed() {
        return Err(access_denied(format!("User: {} is not authorized to perform: sts:GetFederationToken on resource: arn:aws:sts::{acct}:federated-user/{name}", id.caller_arn)));
    }
    let (default, max) = if id.is_root() { (ROOT_MAX_S, ROOT_MAX_S) } else { (st::GET_SESSION_TOKEN_DEFAULT_S, st::GET_SESSION_TOKEN_MAX_S) };
    let d = p.duration(default, max, "")?;
    let policy = p.session_policy()?;
    let arns = p.policy_arns(store, &acct)?;
    let tags = p.tags("Tags")?;
    check_tags(&tags, &[])?;
    let mut s = Session::new(k.permission);
    s.key = Some(k.id.clone());
    s.buckets = k.buckets.clone();
    s.account = acct.clone();
    s.user = k.user.clone();
    s.federated = Some(name.to_string());
    s.policy = policy;
    s.policy_arns = arns;
    s.tags = tags;
    let (s, c) = store.mint_session(s, d).await.map_err(store_failure)?;
    Ok(format!(
        "{}<FederatedUser><FederatedUserId>{}</FederatedUserId><Arn>{}</Arn></FederatedUser><PackedPolicySize>{}</PackedPolicySize>",
        credentials_xml(&c),
        escape(&format!("{acct}:{name}")),
        escape(&s.federated_arn(&acct).unwrap_or_default()),
        packed_size(&s)
    ))
}

/// A `RoleArn`: the role it names, with its account (`None`: no such role).
fn role_of(store: &Store, arn: &str) -> Result<Option<Role>, StsError> {
    let Some((account, path_name)) = arn.strip_prefix("arn:aws:iam::").and_then(|r| r.split_once(":role")) else { return Ok(None) };
    if account.len() != 12 || !account.bytes().all(|b| b.is_ascii_digit()) || !path_name.starts_with('/') {
        return Ok(None);
    }
    let name = path_name.rsplit('/').next().unwrap_or_default();
    if st::validate_role_name(name).is_err() {
        return Ok(None);
    }
    let role = store.role_in(account, name).map_err(store_failure)?;
    // The ARN must be the role's exactly (its path too).
    Ok(role.filter(|r| r.own_arn() == arn))
}

fn check_tags(tags: &[(String, String)], transitive: &[String]) -> Result<(), StsError> {
    if tags.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > st::MAX_TAG_BYTES {
        return Err(StsError::new("PackedPolicyTooLarge", 400, "Packed size of session tags exceeds the limit"));
    }
    st::validate_tags(tags, transitive).map_err(validation)
}

fn packed_size(s: &Session) -> usize {
    let used = s.policy.as_ref().map_or(0, String::len) + s.policy_arns.iter().map(String::len).sum::<usize>() + s.tags.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
    (used * 100).div_ceil(st::MAX_SESSION_POLICY_BYTES + st::MAX_TAG_BYTES)
}

/// A new session of `role` (its account's).
fn role_session(role: &Role, name: &str, policy: Option<String>) -> Session {
    let (permission, buckets) = match role.permission {
        Some(p) => (p, role.buckets.clone()),
        // An IAM role has no permission level: an empty scope covers nothing.
        None => (Permission::Read, Some(Vec::new())),
    };
    let mut s = Session::new(permission);
    s.buckets = buckets;
    s.role = Some(SessionRole { name: role.name.clone(), id: role.id.clone(), generation: role.generation });
    s.name = Some(name.to_string());
    s.policy = policy;
    s.account = role.account.clone();
    s
}

fn assumed_role_xml(s: &Session, c: &st::Credentials) -> String {
    let account = &s.account;
    let mut out = format!(
        "<AssumedRoleUser><Arn>{}</Arn><AssumedRoleId>{}</AssumedRoleId></AssumedRoleUser>{}<PackedPolicySize>{}</PackedPolicySize>",
        escape(&s.assumed_role_arn(account).unwrap_or_default()),
        escape(&s.user_id().unwrap_or_default()),
        credentials_xml(c),
        packed_size(s)
    );
    if let Some(si) = &s.source_identity {
        out.push_str(&format!("<SourceIdentity>{}</SourceIdentity>", escape(si)));
    }
    out
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Condition keys of an STS call (trust policies, and the caller's own policies).
struct StsCond<'a> {
    ctx: &'a Ctx,
    id: Option<&'a Identity>,
    p: &'a Params,
    /// More keys (lowercase): web identity claims, MFA presented with the call.
    extra: Vec<(String, String)>,
}

impl Context for StsCond<'_> {
    fn values(&self, key: &str) -> Option<Vec<String>> {
        if let Some((_, v)) = self.extra.iter().find(|(k, _)| k == key) {
            return Some(vec![v.clone()]);
        }
        if let Some(id) = self.id
            && let Some(v) = id.condition(key, self.ctx.store().now())
        {
            return v;
        }
        let one = |v: Option<&str>| v.map(|v| vec![v.to_string()]);
        if let Some(t) = key.strip_prefix("aws:requesttag/") {
            return self.p.tags("Tags").ok()?.into_iter().find(|(k, _)| k.eq_ignore_ascii_case(t)).map(|(_, v)| vec![v]);
        }
        match key {
            "sts:externalid" => one(self.p.get("ExternalId")),
            "sts:rolesessionname" => one(self.p.get("RoleSessionName")),
            "sts:sourceidentity" => one(self.p.get("SourceIdentity")),
            "aws:tagkeys" => {
                let t = self.p.tags("Tags").ok()?;
                (!t.is_empty()).then(|| t.into_iter().map(|(k, _)| k).collect())
            }
            "aws:sourceip" => self.ctx.peer.map(|p| vec![p.to_string()]),
            "aws:securetransport" => Some(vec![crate::access::secure(self.ctx).to_string()]),
            "aws:currenttime" => Some(vec![s3_proto::time::iso8601_ms(self.ctx.store().now())]),
            "aws:epochtime" => Some(vec![self.ctx.now_s().to_string()]),
            "aws:useragent" => one(self.ctx.h("user-agent")),
            "aws:principaltype" if self.id.is_none() => Some(vec!["WebIdentityUser".into()]),
            _ => None,
        }
    }
}

/// How a role's trust treats a principal: `Some(via)` if it trusts it (a trust policy's
/// `Allow` naming it, or a control-plane trust listing its key), `None` if not. An explicit
/// `Deny` is an error.
fn trusted(role: &Role, who: &Principal, caller_key: Option<&str>, action: &str, cond: &dyn Context) -> Result<Option<Via>, StsError> {
    let denied = || access_denied("Not authorized to perform sts:AssumeRole");
    match &role.iam.trust_policy {
        Some(doc) => {
            let Ok(policy) = Policy::parse_trust(doc.as_bytes(), crate::iam::MAX_TRUST_POLICY_BYTES) else {
                tracing::error!(role = role.name, "a role's trust policy doesn't parse");
                return Err(denied());
            };
            let v = policy.evaluate(who, action, &role.own_arn(), cond);
            if v.deny {
                return Err(denied());
            }
            Ok(v.allow)
        }
        None => Ok(caller_key.filter(|k| role.trust.keys.iter().any(|t| t == k)).map(|_| Via::Direct)),
    }
}

async fn assume_role(ctx: &Ctx, store: &Store, k: &AccessKey, id: &Identity, p: &Params) -> Result<String, StsError> {
    let arn = p.required("RoleArn", "roleArn")?;
    let name = p.required("RoleSessionName", "roleSessionName")?;
    st::validate_session_name(name).map_err(validation)?;
    if let Some(e) = p.get("ExternalId") {
        st::validate_external_id(e).map_err(validation)?;
    }
    let denied = || access_denied(format!("User: {} is not authorized to perform: sts:AssumeRole on resource: {arn}", id.caller_arn));
    if id.dead || matches!(id.kind, Kind::Federated { .. }) {
        return Err(denied());
    }
    let role = role_of(store, arn)?.ok_or_else(denied)?;
    let chained = matches!(id.kind, Kind::Role { .. });
    // MFA presented with the call (the caller's own device).
    let mfa_at = match p.mfa()? {
        Some((serial, code)) => Some(caller_mfa(store, id, serial, code).await?),
        None => id.session.as_ref().and_then(|s| s.mfa_at),
    };
    let mut extra = Vec::new();
    if mfa_at.is_some() {
        extra.push(("aws:multifactorauthpresent".to_string(), "true".to_string()));
        extra.push(("aws:multifactorauthage".to_string(), ((store.now() - mfa_at.unwrap_or_default()).max(0) / 1000).to_string()));
    } else if !k.is_temporary() {
        // Long-term keys without MFA: absent.
    } else {
        extra.push(("aws:multifactorauthpresent".to_string(), "false".to_string()));
    }
    let cond = StsCond { ctx, id: Some(id), p, extra };
    // The trust: an IAM trust policy (AWS doesn't let the root user assume roles with one), or
    // a control-plane trust naming the caller's permanent key.
    let via = if role.iam.trust_policy.is_some() && id.is_root() && k.session.is_none() {
        None
    } else {
        trusted(&role, &id.principal, k.acting_key().filter(|_| !chained), "sts:AssumeRole", &cond)?
    };
    let Some(via) = via else { return Err(denied()) };
    if role.iam.trust_policy.is_none()
        && let Some(want) = &role.trust.external_id
        && !p.get("ExternalId").is_some_and(|got| ct_eq(got, want))
    {
        return Err(denied());
    }
    // The caller's own side: another account's role, or a trust that only names the caller's
    // account, needs its policies to allow sts:AssumeRole on the role (the root user's always
    // do).
    let own = id.verdict("sts:AssumeRole", &role.own_arn(), &cond, false);
    if own.deny {
        return Err(denied());
    }
    let same = id.account.id == role.account;
    if (!same || via == Via::Account) && !own.allowed() {
        return Err(denied());
    }
    if p.get("Tags.member.1.Key").is_some() || p.get("TransitiveTagKeys.member.1").is_some() {
        // Session tags need sts:TagSession in an IAM trust policy, as on AWS.
        if role.iam.trust_policy.is_some() && trusted(&role, &id.principal, None, "sts:TagSession", &cond)?.is_none() {
            return Err(access_denied(format!("User: {} is not authorized to perform: sts:TagSession on resource: {arn}", id.caller_arn)));
        }
    }
    let max = if chained { CHAINED_MAX_S.min(role.max_session_s) } else { role.max_session_s };
    let why = if chained { "The requested DurationSeconds exceeds the 1 hour session limit for roles assuming other roles." } else { "The requested DurationSeconds exceeds the MaxSessionDuration set for this role." };
    if chained && p.get("DurationSeconds").and_then(|d| d.trim().parse::<i64>().ok()).is_some_and(|d| d > CHAINED_MAX_S) {
        return Err(validation(why));
    }
    let d = p.duration(st::ROLE_DEFAULT_SESSION_S, max, why)?;
    let policy = p.session_policy()?;
    let arns = p.policy_arns(store, &role.account)?;
    let mut tags = p.tags("Tags")?;
    let mut transitive: Vec<String> = p.list("TransitiveTagKeys")?;
    let mut source_identity = p.get("SourceIdentity").map(str::to_string);
    if let Some(si) = &source_identity {
        st::validate_source_identity(si).map_err(validation)?;
    }
    // Chaining keeps the caller's transitive tags and source identity, as on AWS.
    if let Some(s) = id.session.as_ref().filter(|_| chained) {
        for key in &s.transitive_tag_keys {
            if let Some((tk, tv)) = s.tags.iter().find(|(t, _)| t.eq_ignore_ascii_case(key)) {
                if tags.iter().any(|(t, _)| t.eq_ignore_ascii_case(tk)) {
                    return Err(validation(format!("the transitive session tag {tk} can't be set again")));
                }
                tags.push((tk.clone(), tv.clone()));
                transitive.push(tk.clone());
            }
        }
        if let (Some(have), Some(want)) = (&s.source_identity, &source_identity)
            && have != want
        {
            return Err(validation("SourceIdentity can't change in a role chain"));
        }
        source_identity = source_identity.or_else(|| s.source_identity.clone());
    }
    check_tags(&tags, &transitive)?;
    let mut s = role_session(&role, name, policy);
    s.key = k.acting_key().map(str::to_string);
    s.tags = tags;
    s.transitive_tag_keys = transitive;
    s.source_identity = source_identity;
    s.policy_arns = arns;
    s.mfa_at = mfa_at;
    let (s, c) = store.mint_session(s, d).await.map_err(store_failure)?;
    Ok(assumed_role_xml(&s, &c))
}

/// The OIDC providers a role trusts: its control-plane trust's, or the account's IAM OIDC
/// providers its trust policy names (`Federated`), with their ARNs.
fn web_providers(store: &Store, role: &Role) -> Result<Vec<(OidcProvider, Option<String>)>, StsError> {
    let Some(doc) = &role.iam.trust_policy else { return Ok(role.trust.oidc.iter().cloned().map(|p| (p, None)).collect()) };
    let Ok(policy) = Policy::parse_trust(doc.as_bytes(), crate::iam::MAX_TRUST_POLICY_BYTES) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for arn in policy.federated_principals() {
        let Some(name) = arn.strip_prefix(&format!("arn:aws:iam::{}:oidc-provider/", role.account)) else { continue };
        if let Some(e) = store.iam_oidc_provider(&role.account, name).map_err(store_failure)? {
            out.push((OidcProvider { issuer: e.url.clone(), audiences: e.client_ids.clone(), subject: None, jwks_uri: None }, Some(arn)));
        }
    }
    Ok(out)
}

async fn assume_role_with_web_identity(ctx: &Ctx, store: &Store, p: &Params) -> Result<String, StsError> {
    let arn = p.required("RoleArn", "roleArn")?;
    let name = p.required("RoleSessionName", "roleSessionName")?;
    let token = p.required("WebIdentityToken", "webIdentityToken")?;
    st::validate_session_name(name).map_err(validation)?;
    if token.len() < 4 || token.len() > oidc::MAX_JWT_BYTES {
        return Err(validation(format!("1 validation error detected: Value at 'webIdentityToken' failed to satisfy constraint: Member must have length less than or equal to {}", oidc::MAX_JWT_BYTES)));
    }
    if p.get("ProviderId").is_some() {
        return Err(validation("ProviderId (OAuth 2.0 access tokens) isn't supported: pass an OpenID Connect ID token."));
    }
    let denied = || access_denied("Not authorized to perform sts:AssumeRoleWithWebIdentity");
    let role = role_of(store, arn)?.ok_or_else(denied)?;
    let providers = web_providers(store, &role)?;
    if providers.is_empty() {
        return Err(denied());
    }
    let d = p.duration(st::ROLE_DEFAULT_SESSION_S, role.max_session_s, "The requested DurationSeconds exceeds the MaxSessionDuration set for this role.")?;
    let policy = p.session_policy()?;
    let arns = p.policy_arns(store, &role.account)?;
    let plain: Vec<OidcProvider> = providers.iter().map(|(p, _)| p.clone()).collect();
    let v = ctx.server.oidc.verify(&plain, token, ctx.now_s()).await.map_err(|e| e.into_sts())?;
    // An IAM trust policy must allow this web identity: its provider, and its conditions on
    // the token's claims.
    if role.iam.trust_policy.is_some() {
        let provider_arn = providers.iter().find(|(p, _)| p.issuer == v.issuer).and_then(|(_, a)| a.clone()).ok_or_else(denied)?;
        let pname = s3_store::sts::provider_name(&v.issuer);
        let who = Principal::Known(Box::new(Requester { account: role.account.clone(), federated: Some(provider_arn.clone()), ..Default::default() }));
        let extra = vec![
            (format!("{}:sub", pname.to_ascii_lowercase()), v.subject.clone()),
            (format!("{}:aud", pname.to_ascii_lowercase()), v.audience.clone()),
            ("aws:federatedprovider".to_string(), provider_arn),
        ];
        let cond = StsCond { ctx, id: None, p, extra };
        if trusted(&role, &who, None, "sts:AssumeRoleWithWebIdentity", &cond)?.is_none() {
            return Err(denied());
        }
    }
    let mut s = role_session(&role, name, policy);
    s.web = Some(WebIdentity { issuer: v.issuer.clone(), subject: v.subject.clone(), audience: v.audience.clone() });
    s.tags = v.tags;
    s.transitive_tag_keys = v.transitive_tag_keys;
    s.source_identity = v.source_identity;
    s.policy_arns = arns;
    let (s, c) = store.mint_session(s, d).await.map_err(store_failure)?;
    Ok(format!(
        "<SubjectFromWebIdentityToken>{}</SubjectFromWebIdentityToken><Audience>{}</Audience>{}<Provider>{}</Provider>",
        escape(&v.subject),
        escape(&v.audience),
        assumed_role_xml(&s, &c),
        escape(&v.issuer)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_and_members() {
        let p = Params(form("Action=AssumeRole&Policy=%7B%22a%22%3A+1%7D&Tags.member.2.Key=b&Tags.member.1.Key=a&Tags.member.1.Value=x&TransitiveTagKeys.member.1=a"));
        assert_eq!(p.get("Policy"), Some("{\"a\": 1}"));
        assert_eq!(p.members("Tags", "Key").unwrap(), vec![(1, "a".to_string()), (2, "b".to_string())]);
        assert_eq!(p.tags("Tags").unwrap(), vec![("a".to_string(), "x".to_string()), ("b".to_string(), String::new())]);
        assert_eq!(p.list("TransitiveTagKeys").unwrap(), vec!["a".to_string()]);
        assert!(Params(form("Tags.member.x.Key=a")).members("Tags", "Key").is_err());
        assert!(Params(form("SerialNumber=arn:aws:iam::1:mfa/x")).mfa().is_err(), "both or neither");
        assert!(Params(form("SerialNumber=arn:aws:iam::123456789012:mfa/x&TokenCode=12345")).mfa().is_err());
        assert!(Params(form("SerialNumber=arn:aws:iam::123456789012:mfa/x&TokenCode=123456")).mfa().unwrap().is_some());
        assert!(validate_federated_name("bob").is_ok());
        assert!(validate_federated_name("b").is_err());
        assert!(validate_federated_name("no spaces").is_err());
    }

    #[test]
    fn durations() {
        let d = |q: &str, def, max, why: &str| Params(form(q)).duration(def, max, why).map_err(|e| e.message);
        assert_eq!(d("", 3600, 7200, "x"), Ok(3600));
        assert_eq!(d("", 3600, 900, "x"), Ok(900), "the default never exceeds the maximum");
        assert_eq!(d("DurationSeconds=900", 3600, 7200, "x"), Ok(900));
        assert!(d("DurationSeconds=899", 3600, 7200, "x").is_err());
        assert_eq!(d("DurationSeconds=7201", 3600, 7200, "MaxSessionDuration").unwrap_err(), "MaxSessionDuration");
        assert!(d("DurationSeconds=x", 3600, 7200, "x").is_err());
    }

    #[test]
    fn expirations() {
        assert_eq!(expiration(1_700_000_000_999), "2023-11-14T22:13:20Z");
    }
}
