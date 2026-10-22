//! The STS query API on the S3 listener, so SDKs pointed at it (`endpoint_url` for `sts`) get
//! temporary credentials: `POST /` (or `GET /?Action=…`) with `Action=…&Version=2011-06-15`,
//! form-encoded, answered in STS's XML.
//!
//! - `GetSessionToken`: temporary credentials of the calling (permanent) key.
//! - `AssumeRole`: a role's, for a key its trust names.
//! - `AssumeRoleWithWebIdentity`: a role's, for an OIDC ID token from a provider its trust
//!   names (unsigned: the token is the proof). See [`oidc`].
//! - `GetCallerIdentity`.
//!
//! Signed calls use SigV4 with service `sts` (any region). Errors are STS's
//! (`<ErrorResponse><Error><Type>…<Code>…`).

pub mod oidc;

use crate::ctx::Ctx;
use crate::temporary;
use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use s3_proto::xml::escape;
use s3_proto::{Code, S3Error};
use s3_store::sts::{self as st, Role, Session, SessionRole, WebIdentity};
use s3_store::{AccessKey, Store};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const XMLNS: &str = "https://sts.amazonaws.com/doc/2011-06-15/";
const VERSION: &str = "2011-06-15";
/// Largest request body: a 20 000-character web identity token, a 2 KB policy and tags fit
/// easily.
const MAX_BODY: usize = 64 * 1024;

/// Whether a request is for STS rather than S3: the service endpoint (`/`, no bucket) with a
/// form body or an `Action` parameter, or a CORS preflight of a `POST` there (S3 has no
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

/// An STS error.
#[derive(Debug)]
pub(crate) struct StsError {
    pub code: &'static str,
    pub status: u16,
    pub message: String,
}

impl StsError {
    fn new(code: &'static str, status: u16, message: impl Into<String>) -> Self {
        Self { code, status, message: message.into() }
    }
}

fn access_denied(m: impl Into<String>) -> StsError {
    StsError::new("AccessDenied", 403, m)
}

fn validation(m: impl Into<String>) -> StsError {
    StsError::new("ValidationError", 400, m)
}

fn malformed_policy(m: impl Into<String>) -> StsError {
    StsError::new("MalformedPolicyDocument", 400, m)
}

/// The STS error for a failed signature check (S3's codes in, STS's out).
fn auth_error(e: S3Error) -> StsError {
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

fn store_failure(e: s3_store::StoreError) -> StsError {
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
    let mut resp = if ctx.method == Method::OPTIONS {
        preflight(ctx)
    } else {
        match run(ctx, body, &request_id).await {
            Ok(xml) => xml_response(StatusCode::OK, xml),
            Err(e) => {
                if e.status >= 500 {
                    tracing::warn!(code = e.code, "sts request failed");
                }
                let status = StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_REQUEST);
                let kind = if e.status >= 500 { "Receiver" } else { "Sender" };
                xml_response(
                    status,
                    format!(
                        "<ErrorResponse xmlns=\"{XMLNS}\"><Error><Type>{kind}</Type><Code>{}</Code><Message>{}</Message></Error><RequestId>{request_id}</RequestId></ErrorResponse>",
                        e.code,
                        escape(&e.message)
                    ),
                )
            }
        }
    };
    // Browsers may call STS directly (web identity); nothing here relies on cookies.
    if ctx.h("origin").is_some() {
        crate::response::set(&mut resp, "access-control-allow-origin", "*");
        crate::response::set(&mut resp, "access-control-expose-headers", "x-amzn-requestid, x-amz-request-id");
    }
    crate::response::set(&mut resp, "x-amzn-requestid", &request_id);
    resp
}

fn request_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("os randomness");
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn xml_response(status: StatusCode, body: String) -> Response {
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
fn form(raw: &str) -> Vec<(String, String)> {
    // Form encoding writes spaces as `+`.
    s3_proto::http::parse_query(&raw.replace('+', "%20"))
}

struct Params(Vec<(String, String)>);

impl Params {
    fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn required(&self, name: &str, field: &str) -> Result<&str, StsError> {
        self.get(name).ok_or_else(|| validation(format!("1 validation error detected: Value null at '{field}' failed to satisfy constraint: Member must not be null")))
    }

    /// `Prefix.member.N.<suffix>` values in member order (`suffix` empty for plain lists).
    fn members(&self, prefix: &str, suffix: &str) -> Result<Vec<(u32, String)>, StsError> {
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

    fn duration(&self, default: i64, max: i64, role_max: bool) -> Result<i64, StsError> {
        let Some(v) = self.get("DurationSeconds") else { return Ok(default.min(max)) };
        let d: i64 = v.trim().parse().map_err(|_| validation(format!("1 validation error detected: Value '{v}' at 'durationSeconds' failed to satisfy constraint: Member must be a number")))?;
        if d < st::MIN_SESSION_S {
            return Err(validation(format!("1 validation error detected: Value '{d}' at 'durationSeconds' failed to satisfy constraint: Member must have value greater than or equal to {}", st::MIN_SESSION_S)));
        }
        if d > max {
            return Err(validation(if role_max {
                "The requested DurationSeconds exceeds the MaxSessionDuration set for this role.".to_string()
            } else {
                format!("1 validation error detected: Value '{d}' at 'durationSeconds' failed to satisfy constraint: Member must have value less than or equal to {max}")
            }));
        }
        Ok(d)
    }

    /// `Policy` (checked and minified) after refusing `PolicyArns`.
    fn session_policy(&self) -> Result<Option<String>, StsError> {
        if self.0.iter().any(|(k, _)| k.starts_with("PolicyArns.")) {
            return Err(validation("PolicyArns (managed session policies) aren't supported: roll has no managed policies. Pass the policy inline in Policy instead."));
        }
        let Some(p) = self.get("Policy") else { return Ok(None) };
        if p.is_empty() || p.len() > st::MAX_SESSION_POLICY_BYTES {
            return Err(validation(format!("1 validation error detected: Value at 'policy' failed to satisfy constraint: Member must have length less than or equal to {}", st::MAX_SESSION_POLICY_BYTES)));
        }
        crate::validate_identity_policy(p, st::MAX_SESSION_POLICY_BYTES).map(Some).map_err(malformed_policy)
    }

    fn no_mfa(&self) -> Result<(), StsError> {
        if self.get("SerialNumber").is_some() || self.get("TokenCode").is_some() {
            return Err(validation("MFA (SerialNumber, TokenCode) isn't supported: roll has no MFA devices."));
        }
        Ok(())
    }
}

async fn run(ctx: &mut Ctx, body: Body, request_id: &str) -> Result<String, StsError> {
    let body = axum::body::to_bytes(body, MAX_BODY).await.map_err(|_| validation(format!("The request body is larger than {MAX_BODY} bytes.")))?;
    let text = std::str::from_utf8(&body).map_err(|_| validation("The request body isn't UTF-8."))?;
    let mut params = form(&ctx.raw_query);
    params.extend(form(text));
    let p = Params(params);
    let action = p.get("Action").ok_or_else(|| StsError::new("MissingAction", 400, "Missing Action"))?.to_string();
    if let Some(v) = p.get("Version")
        && v != VERSION
    {
        return Err(StsError::new("InvalidAction", 400, format!("Could not find operation {action} for version {v}")));
    }
    let store = ctx.store().clone();
    let result = match action.as_str() {
        "AssumeRoleWithWebIdentity" => assume_role_with_web_identity(ctx, &store, &p).await?,
        "GetCallerIdentity" | "GetSessionToken" | "AssumeRole" => {
            let caller = authenticate(ctx, &body)?;
            match action.as_str() {
                "GetCallerIdentity" => caller_identity(&store, &caller),
                "GetSessionToken" => get_session_token(&store, &caller, &p).await?,
                _ => assume_role(&store, &caller, &p).await?,
            }
        }
        "AssumeRoleWithSAML" | "GetFederationToken" | "DecodeAuthorizationMessage" | "GetAccessKeyInfo" | "AssumeRoot" | "GetDelegatedAccessToken" => {
            return Err(StsError::new("InvalidAction", 400, format!("{action} isn't supported by roll (see docs/storage/compatibility.md)")));
        }
        _ => return Err(StsError::new("InvalidAction", 400, format!("Could not find operation {action} for version {VERSION}"))),
    };
    Ok(format!(
        "<{action}Response xmlns=\"{XMLNS}\"><{action}Result>{result}</{action}Result><ResponseMetadata><RequestId>{request_id}</RequestId></ResponseMetadata></{action}Response>"
    ))
}

/// Verify a signed STS call (SigV4, service `sts`). The payload is the body.
fn authenticate(ctx: &Ctx, body: &[u8]) -> Result<AccessKey, StsError> {
    let pairs = ctx.header_pairs();
    let headers: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let req = s3_proto::sigv4::Request { method: ctx.method.as_str(), path: &ctx.raw_path, query: &ctx.raw_query, headers: &headers };
    let hash = hex::encode(Sha256::digest(body));
    let token = temporary::request_token(&ctx.headers, &ctx.query);
    let mut found = None;
    let mut failure = None;
    let r = s3_proto::sigv4::authenticate_service(&req, ctx.now_s(), "sts", &hash, |id| match temporary::resolve(ctx.store(), id, token) {
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
        Ok(Some(_)) => found.ok_or_else(|| StsError::new("InternalFailure", 500, "no key")),
        Ok(None) => Err(StsError::new("MissingAuthenticationToken", 403, "Request is missing Authentication Token")),
        Err(e) => Err(auth_error(match failure {
            Some(f) if e.code == Code::InvalidAccessKeyId => f,
            _ => e,
        })),
    }
}

fn user_arn(account: &str, key: &str) -> String {
    format!("arn:aws:iam::{account}:user/{key}")
}

/// `Arn` and `UserId` of a caller.
fn identity(store: &Store, k: &AccessKey) -> (String, String) {
    let account = store.account_id();
    match &k.session {
        Some(s) if s.role.is_some() => (s.assumed_role_arn(&account).unwrap_or_default(), s.user_id().unwrap_or_default()),
        _ => {
            let id = k.acting_key().unwrap_or(&k.id);
            (user_arn(&account, id), id.to_string())
        }
    }
}

fn caller_identity(store: &Store, k: &AccessKey) -> String {
    let (arn, user_id) = identity(store, k);
    format!("<Arn>{}</Arn><UserId>{}</UserId><Account>{}</Account>", escape(&arn), escape(&user_id), store.account_id())
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

async fn get_session_token(store: &Store, k: &AccessKey, p: &Params) -> Result<String, StsError> {
    if k.is_temporary() {
        return Err(access_denied("Cannot call GetSessionToken with session credentials"));
    }
    p.no_mfa()?;
    let d = p.duration(st::GET_SESSION_TOKEN_DEFAULT_S, st::GET_SESSION_TOKEN_MAX_S, false)?;
    let mut s = Session::new(k.permission);
    s.key = Some(k.id.clone());
    s.buckets = k.buckets.clone();
    let (_, c) = store.mint_session(s, d).await.map_err(store_failure)?;
    Ok(credentials_xml(&c))
}

/// The role a `RoleArn` names (`None` if it isn't one of this account's).
fn role_of(store: &Store, arn: &str) -> Result<Option<Role>, StsError> {
    let account = store.account_id();
    let Some(name) = arn.strip_prefix(&format!("arn:aws:iam::{account}:role/")) else { return Ok(None) };
    if st::validate_role_name(name).is_err() {
        return Ok(None);
    }
    store.role(name).map_err(store_failure)
}

fn session_tags(p: &Params) -> Result<st::SessionTags, StsError> {
    let keys = p.members("Tags", "Key")?;
    let values = p.members("Tags", "Value")?;
    let mut tags = Vec::with_capacity(keys.len());
    for (n, k) in keys {
        let v = values.iter().find(|(m, _)| *m == n).map(|(_, v)| v.clone()).unwrap_or_default();
        tags.push((k, v));
    }
    let transitive: Vec<String> = p.members("TransitiveTagKeys", "")?.into_iter().map(|(_, v)| v).collect();
    if tags.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > st::MAX_TAG_BYTES {
        return Err(StsError::new("PackedPolicyTooLarge", 400, "Packed size of session tags exceeds the limit"));
    }
    st::validate_tags(&tags, &transitive).map_err(validation)?;
    Ok((tags, transitive))
}

fn packed_size(s: &Session) -> usize {
    let used = s.policy.as_ref().map_or(0, String::len) + s.tags.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
    (used * 100).div_ceil(st::MAX_SESSION_POLICY_BYTES + st::MAX_TAG_BYTES)
}

fn role_session(role: &Role, name: &str, policy: Option<String>) -> Session {
    let mut s = Session::new(role.permission);
    s.buckets = role.buckets.clone();
    s.role = Some(SessionRole { name: role.name.clone(), id: role.id.clone(), generation: role.generation });
    s.name = Some(name.to_string());
    s.policy = policy;
    s
}

fn assumed_role_xml(store: &Store, s: &Session, c: &st::Credentials) -> String {
    let account = store.account_id();
    let mut out = format!(
        "<AssumedRoleUser><Arn>{}</Arn><AssumedRoleId>{}</AssumedRoleId></AssumedRoleUser>{}<PackedPolicySize>{}</PackedPolicySize>",
        escape(&s.assumed_role_arn(&account).unwrap_or_default()),
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

async fn assume_role(store: &Store, k: &AccessKey, p: &Params) -> Result<String, StsError> {
    let arn = p.required("RoleArn", "roleArn")?;
    let name = p.required("RoleSessionName", "roleSessionName")?;
    st::validate_session_name(name).map_err(validation)?;
    p.no_mfa()?;
    let (caller_arn, _) = identity(store, k);
    let denied = || access_denied(format!("User: {caller_arn} is not authorized to perform: sts:AssumeRole on resource: {arn}"));
    // Role chaining isn't supported: a role session can't assume another role.
    let Some(caller) = k.acting_key() else { return Err(denied()) };
    let role = role_of(store, arn)?.ok_or_else(denied)?;
    if !role.trust.keys.iter().any(|t| t == caller) {
        return Err(denied());
    }
    if let Some(want) = &role.trust.external_id
        && !p.get("ExternalId").is_some_and(|got| ct_eq(got, want))
    {
        return Err(denied());
    }
    if let Some(e) = p.get("ExternalId") {
        st::validate_external_id(e).map_err(validation)?;
    }
    let d = p.duration(st::ROLE_DEFAULT_SESSION_S, role.max_session_s, true)?;
    let policy = p.session_policy()?;
    let (tags, transitive) = session_tags(p)?;
    let mut s = role_session(&role, name, policy);
    s.key = Some(caller.to_string());
    s.tags = tags;
    s.transitive_tag_keys = transitive;
    if let Some(si) = p.get("SourceIdentity") {
        st::validate_source_identity(si).map_err(validation)?;
        s.source_identity = Some(si.to_string());
    }
    let (s, c) = store.mint_session(s, d).await.map_err(store_failure)?;
    Ok(assumed_role_xml(store, &s, &c))
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
    if role.trust.oidc.is_empty() {
        return Err(denied());
    }
    let d = p.duration(st::ROLE_DEFAULT_SESSION_S, role.max_session_s, true)?;
    let policy = p.session_policy()?;
    let v = ctx.server.oidc.verify(&role.trust.oidc, token, ctx.now_s()).await.map_err(|e| e.into_sts())?;
    let mut s = role_session(&role, name, policy);
    s.web = Some(WebIdentity { issuer: v.issuer.clone(), subject: v.subject.clone(), audience: v.audience.clone() });
    s.tags = v.tags;
    s.transitive_tag_keys = v.transitive_tag_keys;
    s.source_identity = v.source_identity;
    let (s, c) = store.mint_session(s, d).await.map_err(store_failure)?;
    Ok(format!(
        "<SubjectFromWebIdentityToken>{}</SubjectFromWebIdentityToken><Audience>{}</Audience>{}<Provider>{}</Provider>",
        escape(&v.subject),
        escape(&v.audience),
        assumed_role_xml(store, &s, &c),
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
        let (tags, transitive) = session_tags(&p).unwrap();
        assert_eq!(tags, vec![("a".to_string(), "x".to_string()), ("b".to_string(), String::new())]);
        assert_eq!(transitive, vec!["a".to_string()]);
        assert!(Params(form("Tags.member.x.Key=a")).members("Tags", "Key").is_err());
        assert_eq!(Params(form("PolicyArns.member.1.arn=arn:aws:iam::aws:policy/X")).session_policy().unwrap_err().code, "ValidationError");
    }

    #[test]
    fn durations() {
        let d = |q: &str, def, max, role| Params(form(q)).duration(def, max, role).map_err(|e| e.message);
        assert_eq!(d("", 3600, 7200, true), Ok(3600));
        assert_eq!(d("", 3600, 900, true), Ok(900), "the default never exceeds the role's maximum");
        assert_eq!(d("DurationSeconds=900", 3600, 7200, true), Ok(900));
        assert!(d("DurationSeconds=899", 3600, 7200, true).is_err());
        assert!(d("DurationSeconds=7201", 3600, 7200, true).unwrap_err().contains("MaxSessionDuration"));
        assert!(d("DurationSeconds=x", 3600, 7200, true).is_err());
    }

    #[test]
    fn expirations() {
        assert_eq!(expiration(1_700_000_000_999), "2023-11-14T22:13:20Z");
    }
}
