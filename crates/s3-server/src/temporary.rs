//! Temporary credentials (STS sessions) on S3 requests: the session token, the principal and
//! condition keys a session has, and the bound its policies put on everything it does.
//!
//! - A temporary key (`RT…`) needs its session token (`x-amz-security-token`, or
//!   `X-Amz-Security-Token` in a presigned URL or POST form); without it the key is unknown
//!   (`InvalidAccessKeyId`, as on AWS). A permanent key with a token is `InvalidToken`.
//! - The token is checked without any lookup but the role's (see `s3_store::sts`): tampered or
//!   foreign tokens are `InvalidToken`, expired ones `ExpiredToken`, revoked ones `InvalidToken`.
//! - A role session's permission and scope are the role's (narrowed when it was issued). On
//!   top, the role's inline policy and the session policy must both allow every action, be it
//!   allowed by the permission, a bucket policy or an ACL: a session never does more than
//!   they say.

use crate::error::store_error;
use crate::policy::{Account, Context, Decision, Policy, Principal};
use s3_proto::{Code, S3Error};
use s3_store::sts::{MAX_ROLE_POLICY_BYTES, MAX_SESSION_POLICY_BYTES, SessionError};
use s3_store::{AccessKey, Store, is_temporary_key_id};

/// The session token of a request: the `x-amz-security-token` header, or the query parameter
/// of a presigned URL. An empty one counts as none.
pub(crate) fn request_token<'a>(headers: &'a axum::http::HeaderMap, query: &'a [(String, String)]) -> Option<&'a str> {
    let h = headers.get("x-amz-security-token").and_then(|v| v.to_str().ok());
    let q = || query.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-amz-security-token")).map(|(_, v)| v.as_str());
    h.or_else(q).map(str::trim).filter(|t| !t.is_empty())
}

/// The key `access_key_id` names, with `token` its session token if the request has one.
pub(crate) fn resolve(store: &Store, access_key_id: &str, token: Option<&str>) -> Result<AccessKey, S3Error> {
    let unknown = || S3Error::new(Code::InvalidAccessKeyId).with("AWSAccessKeyId", access_key_id.to_string());
    match token {
        Some(t) if is_temporary_key_id(access_key_id) => store.session_key(access_key_id, t).map_err(session_error),
        Some(_) => Err(S3Error::new(Code::InvalidToken)),
        None => match store.access_key(access_key_id) {
            Ok(Some(k)) => Ok(k),
            Ok(None) => Err(unknown()),
            Err(e) => {
                tracing::warn!(error = %e, "looking up an access key failed");
                Err(store_error(e))
            }
        },
    }
}

fn session_error(e: SessionError) -> S3Error {
    match e {
        SessionError::Invalid => S3Error::new(Code::InvalidToken),
        SessionError::Expired => S3Error::new(Code::ExpiredToken),
        SessionError::Revoked => S3Error::msg(Code::InvalidToken, "The provided token has been revoked."),
        SessionError::Store(e) => store_error(e),
    }
}

/// Who a key is to bucket policies and ACLs: a role session answers to its role's and its own
/// assumed-role ARN; a `GetSessionToken` session is its key.
pub(crate) fn principal(store: &Store, k: &AccessKey) -> Principal {
    match &k.session {
        Some(s) if s.role.is_some() => {
            let account = store.account_id();
            let arns = [s.role_arn(&account), s.assumed_role_arn(&account)].into_iter().flatten().collect();
            Principal::Session { id: k.id.clone(), canonical: store.canonical_user(&k.id), arns }
        }
        _ => {
            let id = k.acting_key().unwrap_or(&k.id);
            Principal::Key { id: id.to_string(), canonical: store.canonical_user(id) }
        }
    }
}

/// The condition keys a session sets (`key` is lowercase). `None`: not a session's key (or not
/// a session); `Some(None)`: the key is absent for this session.
pub(crate) fn condition(k: &AccessKey, account: &str, key: &str) -> Option<Option<Vec<String>>> {
    let s = k.session.as_ref()?;
    let one = |v: Option<String>| Some(v.map(|v| vec![v]));
    if let Some(name) = key.strip_prefix("aws:principaltag/") {
        return one(s.tags.iter().find(|(t, _)| t.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()));
    }
    match key {
        "aws:tokenissuetime" => return one(Some(s3_proto::time::iso8601_ms(s.issued_at))),
        "aws:multifactorauthpresent" => return one(Some("false".into())),
        "aws:sourceidentity" => return one(s.source_identity.clone()),
        "aws:federatedprovider" => return one(s.web.as_ref().map(|w| format!("arn:aws:iam::{account}:oidc-provider/{}", w.provider()))),
        _ => {}
    }
    if let Some(w) = &s.web {
        let provider = w.provider().to_ascii_lowercase();
        if let Some(claim) = key.strip_prefix(provider.as_str()).and_then(|r| r.strip_prefix(':')) {
            return match claim {
                "sub" => one(Some(w.subject.clone())),
                "aud" => one(Some(w.audience.clone())),
                _ => Some(None),
            };
        }
    }
    if s.role.is_some() {
        return match key {
            "aws:principalarn" => one(s.role_arn(account)),
            "aws:userid" => one(s.user_id()),
            "aws:username" => Some(None),
            "aws:principaltype" => one(Some("AssumedRole".into())),
            _ => None,
        };
    }
    // A `GetSessionToken` session is its key.
    let parent = s.key.clone();
    match key {
        "aws:userid" | "aws:username" => one(parent),
        "aws:principalarn" => one(parent.map(|p| format!("arn:aws:iam::{account}:user/{p}"))),
        _ => None,
    }
}

/// Whether a session's policies (its role's inline policy and its session policy) allow
/// `action` on `resource`. Permanent keys: always.
pub(crate) fn bounded(k: &AccessKey, account: &Account, action: &str, resource: &str, cond: &dyn Context) -> bool {
    let Some(s) = &k.session else { return true };
    for (doc, max) in [(s.role_policy.as_deref(), MAX_ROLE_POLICY_BYTES), (s.policy.as_deref(), MAX_SESSION_POLICY_BYTES)] {
        let Some(doc) = doc else { continue };
        // Both were valid when they were set; if one no longer parses, deny.
        let Ok(p) = Policy::parse_identity(doc.as_bytes(), max) else {
            tracing::error!(access_key = k.id, "a session's policy doesn't parse");
            return false;
        };
        if p.evaluate(&Principal::Anonymous, account, action, resource, cond) != Decision::Allow {
            return false;
        }
    }
    true
}

/// A query string with session tokens blanked out (for access logs).
pub(crate) fn redact_query(raw: &str) -> String {
    raw.split('&')
        .map(|p| match p.split_once('=') {
            Some((k, _)) if k.eq_ignore_ascii_case("x-amz-security-token") => format!("{k}=REDACTED"),
            _ => p.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction() {
        assert_eq!(redact_query("a=1&X-Amz-Security-Token=secret&b=2"), "a=1&X-Amz-Security-Token=REDACTED&b=2");
        assert_eq!(redact_query("x-amz-security-token=s"), "x-amz-security-token=REDACTED");
        assert_eq!(redact_query("prefix=a"), "prefix=a");
    }

    #[test]
    fn tokens_from_requests() {
        let mut h = axum::http::HeaderMap::new();
        let q = vec![("X-Amz-Security-Token".to_string(), "fromquery".to_string())];
        assert_eq!(request_token(&h, &q), Some("fromquery"));
        h.insert("x-amz-security-token", "fromheader".parse().unwrap());
        assert_eq!(request_token(&h, &q), Some("fromheader"));
        h.insert("x-amz-security-token", " ".parse().unwrap());
        assert_eq!(request_token(&h, &[]), None);
    }
}
