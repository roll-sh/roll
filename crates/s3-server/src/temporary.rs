//! Credentials on S3 requests: which key a request names, and its session token.
//!
//! - A temporary key (`RT…`) needs its session token (`x-amz-security-token`, or
//!   `X-Amz-Security-Token` in a presigned URL or POST form); without it the key is unknown
//!   (`InvalidAccessKeyId`, as on AWS). A permanent key with a token is `InvalidToken`.
//! - The token is checked without any lookup but the role's (see `s3_store::sts`): tampered or
//!   foreign tokens are `InvalidToken`, expired ones `ExpiredToken`. A session whose role was
//!   deleted or that was revoked authenticates, and is then denied everything
//!   (`AccessDenied`), as on AWS (see `identity.rs`).
//! - An inactive key (IAM's `UpdateAccessKey`) is unknown (`InvalidAccessKeyId`), as on AWS.

use crate::error::store_error;
use s3_proto::{Code, S3Error};
use s3_store::sts::SessionError;
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
            Ok(Some(k)) if k.active => Ok(k),
            Ok(_) => Err(unknown()),
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
        SessionError::Revoked => S3Error::new(Code::AccessDenied),
        SessionError::Store(e) => store_error(e),
    }
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
