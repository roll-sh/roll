//! AWS IAM logins for workload identities (docs/secrets.md, "AWS IAM"), as Vault's AWS auth (its
//! IAM method) and Infisical's AWS Auth do: a machine on AWS signs an `sts:GetCallerIdentity`
//! request with its own credentials (an instance profile, a task role, IRSA, Pod Identity, a
//! user's keys) without sending it, and hands the signed request to roll. roll checks it's exactly
//! that request to an STS endpoint, with this server's `X-Roll-Server-ID` among its signed headers
//! and dated within minutes, sends it to STS itself, and reads back who signed it. Nothing has to
//! be turned on in the account, and roll holds no AWS credential. A signed request logs in once:
//! its signature is kept (`aws_logins`, shared by every node) until its date is out of the window.
//!
//! An identity takes one role (every session of it, or sessions whose name starts with a prefix)
//! or one user, in one account: an ARN with a wildcard anywhere else is refused, as a shared OIDC
//! issuer's subject must name one tenant.

use crate::{Error, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The one request a login may carry.
pub const BODY: &str = "Action=GetCallerIdentity&Version=2011-06-15";
pub const SERVER_ID_HEADER: &str = "x-roll-server-id";
/// How far a signed request's date may be from roll's clock, either way.
pub const SKEW_SECS: i64 = 300;
/// The most of STS's answer read.
pub const ANSWER_MAX: usize = 64 * 1024;
const HEADER_MAX: usize = 8 * 1024;
const HEADERS_MAX: usize = 16;

/// The headers sent on to STS. Host comes from the URL roll rebuilds, Content-Length from the body.
const FORWARDED: &[&str] = &["authorization", "content-type", "x-amz-date", "x-amz-security-token", "x-amz-content-sha256", SERVER_ID_HEADER];
/// The headers a signature may cover: what's forwarded, Host and Content-Length.
const SIGNABLE: &[&str] = &["host", "content-length", "content-type", "x-amz-date", "x-amz-security-token", "x-amz-content-sha256", SERVER_ID_HEADER];

/// A signed `GetCallerIdentity` as the client would have sent it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// A request checked to be `GetCallerIdentity` to STS: the endpoint's host and the headers that
/// go with it (lowercase names).
#[derive(Debug, PartialEq, Eq)]
pub struct StsCall {
    pub host: String,
    pub headers: Vec<(String, String)>,
    /// The signature's SHA-256 (hex): what makes a signed request log in only once.
    pub replay_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Role,
    User,
}

/// Who signed, as STS answered: a role's session, or a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// STS's ARN as it answered it.
    pub arn: String,
    partition: String,
    account: String,
    kind: Kind,
    name: String,
    session: Option<String>,
}

/// What an identity takes: one role or user in one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    partition: String,
    account: String,
    kind: Kind,
    name: String,
    session: Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Session {
    Any,
    Exact(String),
    Prefix(String),
}

const PARTITIONS: &[&str] = &["aws", "aws-cn", "aws-us-gov"];

/// IAM's characters for role, user and session names.
fn iam_name(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || "+=,.@_-".contains(c))
}

/// `arn:<partition>:<service>::<account>:<resource>`: the partition, the service, the account and
/// the resource.
fn split_arn(arn: &str) -> Option<(&str, &str, &str, &str)> {
    let mut p = arn.splitn(6, ':');
    let (Some("arn"), Some(partition), Some(service), Some(""), Some(account), Some(resource)) = (p.next(), p.next(), p.next(), p.next(), p.next(), p.next()) else {
        return None;
    };
    let account_ok = account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit());
    (PARTITIONS.contains(&partition) && account_ok).then_some((partition, service, account, resource))
}

/// `role/<path/>name` or `user/<path/>name`: the name. Role and user names are unique in an
/// account whatever their path, and an assumed role's ARN drops the path, so it's only checked.
fn named(resource: &str, kind: &str) -> Option<String> {
    let rest = resource.strip_prefix(kind)?.strip_prefix('/')?;
    let mut parts: Vec<&str> = rest.split('/').collect();
    let name = parts.pop()?;
    (iam_name(name, 64) && parts.iter().all(|p| iam_name(p, 512))).then(|| name.to_string())
}

impl Pattern {
    /// An identity's ARN: a role (`arn:aws:iam::123456789012:role/app`, any of its sessions), one
    /// session or a prefix of them (`arn:aws:sts::123456789012:assumed-role/app/i-*`), or a user
    /// (`arn:aws:iam::123456789012:user/ci`). A `*` only ends a session name.
    pub fn parse(arn: &str) -> std::result::Result<Pattern, String> {
        const SHAPE: &str = "arn is one role or user in one account: arn:aws:iam::<account>:role/<name>, arn:aws:sts::<account>:assumed-role/<role>/<session, may end in *>, or arn:aws:iam::<account>:user/<name>";
        let (partition, service, account, resource) = split_arn(arn.trim()).ok_or(SHAPE)?;
        let (kind, name, session) = match service {
            "iam" if resource.starts_with("role/") => (Kind::Role, named(resource, "role").ok_or(SHAPE)?, Session::Any),
            "iam" if resource.starts_with("user/") => (Kind::User, named(resource, "user").ok_or(SHAPE)?, Session::Any),
            "sts" => {
                let mut p = resource.split('/');
                let (Some("assumed-role"), Some(role), Some(session), None) = (p.next(), p.next(), p.next(), p.next()) else { return Err(SHAPE.into()) };
                if !iam_name(role, 64) {
                    return Err(SHAPE.into());
                }
                let session = match session.strip_suffix('*') {
                    Some("") => Session::Any,
                    Some(prefix) if iam_name(prefix, 63) => Session::Prefix(prefix.to_string()),
                    None if iam_name(session, 64) => Session::Exact(session.to_string()),
                    _ => return Err(SHAPE.into()),
                };
                (Kind::Role, role.to_string(), session)
            }
            _ => return Err(SHAPE.into()),
        };
        Ok(Pattern { partition: partition.into(), account: account.into(), kind, name, session })
    }

    pub fn matches(&self, p: &Principal) -> bool {
        let session = match (&self.session, &p.session) {
            (Session::Any, _) => true,
            (Session::Exact(want), Some(have)) => want == have,
            (Session::Prefix(want), Some(have)) => have.starts_with(want.as_str()),
            _ => false,
        };
        self.partition == p.partition && self.account == p.account && self.kind == p.kind && self.name == p.name && session
    }
}

impl Principal {
    /// The ARN `GetCallerIdentity` answers: an assumed role's session (an instance profile, a task
    /// role, IRSA, Pod Identity, `sts:AssumeRole`) or a user. An account's root and federated
    /// users aren't identities' to take.
    pub fn parse(arn: &str) -> Option<Principal> {
        let (partition, service, account, resource) = split_arn(arn)?;
        let (kind, name, session) = match service {
            "sts" => {
                let mut p = resource.split('/');
                let (Some("assumed-role"), Some(role), Some(session), None) = (p.next(), p.next(), p.next(), p.next()) else { return None };
                (iam_name(role, 64) && iam_name(session, 64)).then_some(())?;
                (Kind::Role, role.to_string(), Some(session.to_string()))
            }
            "iam" => (Kind::User, named(resource, "user")?, None),
            _ => return None,
        };
        Some(Principal { arn: arn.to_string(), partition: partition.into(), account: account.into(), kind, name, session })
    }
}

/// `sts.amazonaws.com`, `sts.<region>.amazonaws.com`, `sts-fips.<region>.amazonaws.com`, or
/// `sts.cn-<…>.amazonaws.com.cn`: the only hosts a login's request goes to.
pub fn sts_host(host: &str) -> bool {
    if host == "sts.amazonaws.com" {
        return true;
    }
    let region = |r: &str| {
        let parts: Vec<&str> = r.split('-').collect();
        let (first, last) = (parts[0], parts[parts.len() - 1]);
        parts.len() >= 3
            && first.len() == 2
            && first.bytes().all(|b| b.is_ascii_lowercase())
            && (1..=2).contains(&last.len())
            && last.bytes().all(|b| b.is_ascii_digit())
            && parts[1..parts.len() - 1].iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()))
    };
    let Some(rest) = host.strip_prefix("sts.").or_else(|| host.strip_prefix("sts-fips.")) else { return false };
    if let Some(r) = rest.strip_suffix(".amazonaws.com.cn") {
        return r.starts_with("cn-") && region(r) && host.starts_with("sts.");
    }
    rest.strip_suffix(".amazonaws.com").is_some_and(|r| region(r) && !r.starts_with("cn-"))
}

/// `YYYYMMDDTHHMMSSZ` as Unix seconds.
fn amz_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' || !s[..8].bytes().chain(s[9..15].bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s[r].parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(4..6)?, n(6..8)?, n(9..11)?, n(11..13)?, n(13..15)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn bad(m: impl Into<String>) -> Error {
    Error::BadRequest(m.into())
}

/// Check a login's request is `GetCallerIdentity` to an STS endpoint, signed over this server's
/// ID (`server_id`) and dated within [`SKEW_SECS`] of `now` (Unix seconds). Nothing is sent before
/// this passes, and what's sent is rebuilt from it: never a URL as the client gave it.
pub fn check(r: &SignedRequest, server_id: &str, now: i64) -> Result<StsCall> {
    if r.method != "POST" {
        return Err(bad("the signed request is a POST of GetCallerIdentity"));
    }
    let not_sts = || bad("the signed request goes to STS: https://sts.amazonaws.com/ or https://sts.<region>.amazonaws.com/");
    let rest = r.url.strip_prefix("https://").ok_or_else(not_sts)?;
    let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
    if !path.is_empty() || !sts_host(host) {
        return Err(not_sts());
    }
    if r.body != BODY {
        return Err(bad(format!("the signed request's body is exactly {BODY}")));
    }
    if r.headers.len() > HEADERS_MAX {
        return Err(bad(format!("the signed request has at most {HEADERS_MAX} headers")));
    }
    let mut headers: BTreeMap<String, &str> = BTreeMap::new();
    for (k, v) in &r.headers {
        let name_ok = !k.is_empty() && k.len() <= 64 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !name_ok || v.len() > HEADER_MAX || v.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(bad(format!("the signed request's header {k} isn't one roll can send")));
        }
        if headers.insert(k.to_ascii_lowercase(), v.as_str()).is_some() {
            return Err(bad(format!("the signed request names {k} twice")));
        }
    }
    if headers.get("host").is_some_and(|h| !h.eq_ignore_ascii_case(host)) {
        return Err(bad("the signed request's Host isn't its URL's"));
    }
    let auth = headers.get("authorization").and_then(|a| a.strip_prefix("AWS4-HMAC-SHA256 ")).ok_or_else(|| Error::Unauthorized("the request isn't signed with AWS Signature Version 4".into()))?;
    // Exactly Credential, SignedHeaders and Signature, once each: the whole header goes to STS, so
    // roll reads the same Signature STS checks (the replay key), never one of two.
    let shape = || bad("the signed request's Authorization is Credential=…, SignedHeaders=…, Signature=…");
    let mut parts: BTreeMap<&str, &str> = BTreeMap::new();
    for p in auth.split(',').map(str::trim) {
        let (name, value) = p.split_once('=').ok_or_else(shape)?;
        if parts.keys().any(|n| n.eq_ignore_ascii_case(name)) {
            return Err(bad(format!("the signed request's Authorization names {name} twice")));
        }
        parts.insert(name, value);
    }
    if !parts.keys().copied().eq(["Credential", "Signature", "SignedHeaders"]) {
        return Err(shape());
    }
    let field = |name: &str| parts.get(name).copied();
    let signed: Vec<&str> = field("SignedHeaders").map(|s| s.split(';').collect()).unwrap_or_default();
    let scope_ok = field("Credential").is_some_and(|c| c.split('/').nth(3) == Some("sts"));
    let signature = field("Signature").filter(|s| s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).ok_or_else(|| Error::Unauthorized("the request's Signature isn't 64 lowercase hex characters".into()))?;
    if !scope_ok {
        return Err(Error::Unauthorized("the request isn't signed for STS".into()));
    }
    if let Some(h) = signed.iter().find(|h| !SIGNABLE.contains(h)) {
        return Err(bad(format!("the signature covers {h}, a header roll doesn't send")));
    }
    for h in ["host", "x-amz-date", SERVER_ID_HEADER] {
        if !signed.contains(&h) {
            return Err(Error::Unauthorized(format!("the signature has to cover {h}")));
        }
    }
    // A request signed for another server (another roll, a Vault) can't be replayed here.
    let named = headers.get(SERVER_ID_HEADER).map(|v| v.trim().trim_end_matches('/'));
    let ours = server_id.trim_end_matches('/');
    if named != Some(ours) {
        return Err(Error::Unauthorized(format!("the signed request names {}, not this server: sign it with X-Roll-Server-ID: {ours} (the client's ROLL_AWS_SERVER_ID; this server's is ROLL_SECRETS_AWS_SERVER_ID)", named.map_or_else(|| "no server".to_string(), |n| format!("server {n}")))));
    }
    let at = headers.get("x-amz-date").and_then(|d| amz_date(d)).ok_or_else(|| Error::Unauthorized("the signed request has no X-Amz-Date".into()))?;
    if (now - at).abs() > SKEW_SECS {
        return Err(Error::Unauthorized(format!("the signed request is dated {}s from roll's clock (at most {SKEW_SECS}s): sign a new one", now - at)));
    }
    let headers = FORWARDED.iter().filter_map(|h| headers.get(*h).map(|v| (h.to_string(), v.to_string()))).collect();
    let replay_key = Sha256::digest(signature.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    Ok(StsCall { host: host.to_ascii_lowercase(), headers, replay_key })
}

/// The element `tag`'s text, when the document has exactly one.
fn element<'a>(doc: &'a str, tag: &str) -> Option<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let start = doc.find(&open)? + open.len();
    let len = doc[start..].find(&close)?;
    (doc.matches(&open).count() == 1).then(|| &doc[start..start + len])
}

/// Who signed, from STS's answer to `GetCallerIdentity`.
pub fn answer(status: u16, body: &[u8]) -> Result<Principal> {
    let doc = std::str::from_utf8(body).map_err(|_| Error::Link("STS answered something that isn't text".into()))?;
    if status != 200 {
        let code: String = element(doc, "Code").unwrap_or_default().chars().filter(char::is_ascii_alphanumeric).take(64).collect();
        let code = if code.is_empty() { status.to_string() } else { code };
        return Err(if status >= 500 { Error::Link(format!("STS answered {code}")) } else { Error::Unauthorized(format!("STS refused the signed request: {code}")) });
    }
    let result = element(doc, "GetCallerIdentityResult").ok_or_else(|| Error::Link("STS's answer has no GetCallerIdentityResult".into()))?;
    let arn = element(result, "Arn").ok_or_else(|| Error::Link("STS's answer has no Arn".into()))?.trim();
    let p = Principal::parse(arn).ok_or_else(|| Error::Forbidden(format!("{arn} isn't a role's session or a user: identities take those")))?;
    if element(result, "Account").is_some_and(|a| a.trim() != p.account) {
        return Err(Error::Link("STS's answer names two accounts".into()));
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_name_one_role_or_user_in_one_account() {
        for ok in [
            "arn:aws:iam::123456789012:role/app",
            "arn:aws:iam::123456789012:role/team/app",
            "arn:aws:sts::123456789012:assumed-role/app/i-*",
            "arn:aws:sts::123456789012:assumed-role/app/*",
            "arn:aws-cn:iam::123456789012:user/ci",
        ] {
            assert!(Pattern::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "arn:aws:iam::*:role/app",
            "arn:aws:iam::123456789012:role/*",
            "arn:aws:iam::123456789012:role/app*",
            "arn:aws:sts::123456789012:assumed-role/*/x",
            "arn:aws:iam::123456789012:root",
            "arn:aws:iam::12345678901:role/app",
            "arn:aws:iam:us-east-1:123456789012:role/app",
            "arn:aws:sts::123456789012:federated-user/bob",
            "arn:other:iam::123456789012:role/app",
        ] {
            assert!(Pattern::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sessions_match_their_role_and_a_prefix_only_its_sessions() {
        let who = |arn: &str| Principal::parse(arn).unwrap();
        let role = Pattern::parse("arn:aws:iam::123456789012:role/team/app").unwrap();
        assert!(role.matches(&who("arn:aws:sts::123456789012:assumed-role/app/i-0abc")), "the path isn't in a session's ARN");
        assert!(!role.matches(&who("arn:aws:sts::123456789012:assumed-role/app2/i-0abc")));
        assert!(!role.matches(&who("arn:aws:iam::123456789012:user/app")));
        let instances = Pattern::parse("arn:aws:sts::123456789012:assumed-role/app/i-*").unwrap();
        assert!(instances.matches(&who("arn:aws:sts::123456789012:assumed-role/app/i-0abc")));
        assert!(!instances.matches(&who("arn:aws:sts::123456789012:assumed-role/app/eks-shop-web")));
        let user = Pattern::parse("arn:aws:iam::123456789012:user/ci").unwrap();
        assert!(user.matches(&who("arn:aws:iam::123456789012:user/deploy/ci")));
        assert!(Principal::parse("arn:aws:iam::123456789012:root").is_none());
    }

    #[test]
    fn only_sts_hosts() {
        for ok in ["sts.amazonaws.com", "sts.eu-west-1.amazonaws.com", "sts-fips.us-gov-west-1.amazonaws.com", "sts.cn-north-1.amazonaws.com.cn", "sts.ap-southeast-5.amazonaws.com"] {
            assert!(sts_host(ok), "{ok}");
        }
        for bad in ["sts.amazonaws.com.evil.com", "evil.com", "sts.eu-west-1.amazonaws.com:443", "sts.169.254.169.254.amazonaws.com", "sts.us-east-1.amazonaws.com.cn", "sts..amazonaws.com", "s3.us-east-1.amazonaws.com"] {
            assert!(!sts_host(bad), "{bad}");
        }
    }

    #[test]
    fn amz_dates() {
        assert_eq!(amz_date("20150830T123600Z"), Some(1_440_938_160));
        assert_eq!(amz_date("19700101T000000Z"), Some(0));
        assert_eq!(amz_date("2015-08-30T12:36:00Z"), None);
    }
}
