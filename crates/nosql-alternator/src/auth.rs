//! AWS Signature Version 4 verification, as Alternator does it.
//!
//! The access key id is a CQL role name and the secret access key is that role's salted hash
//! (`SELECT salted_hash FROM system.roles WHERE role = ...`), supplied by [`Credentials`]. Any
//! region and service in the credential scope are accepted (clients sign for whatever region
//! they are configured with). The signature covers the body as sent (before decompression).
//!
//! Failures, by `__type`:
//! - `MissingAuthenticationTokenException`: no `Authorization` header.
//! - `InvalidSignatureException`: a malformed header, an algorithm other than
//!   `AWS4-HMAC-SHA256`, a missing or malformed `X-Amz-Date`, or a date outside the allowed
//!   clock skew (`Signature expired` / `Signature not yet current`).
//! - `UnrecognizedClientException`: an unknown role, a role that cannot log in, or a signature
//!   mismatch (a wrong secret, a changed body or signed header; `test_cql_rbac.py::test_new_role`).
//!
//! ScyllaDB's own test suite only checks the exception types and the `Signature expired`,
//! `Signature not yet current` and `login=false` fragments; the remaining texts follow DynamoDB.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::{Error, ErrorKind, Result};
use crate::http::HttpRequest;

/// A role's signing secret.
#[derive(Clone, Debug)]
pub struct RoleSecret {
    /// The role's `salted_hash`, used verbatim as the secret access key.
    pub salted_hash: String,
    pub can_login: bool,
}

/// Looks up the secret of a role (the access key id).
#[async_trait]
pub trait Credentials: Send + Sync + 'static {
    async fn secret(&self, role: &str) -> Result<Option<RoleSecret>>;

    /// The role an access key id authenticates as (the key itself, unless it also names
    /// something else, like the database in `role@db`).
    fn role_of(&self, access_key: &str) -> String {
        access_key.to_string()
    }
}

/// A fixed set of credentials, for tests and single-user setups.
#[derive(Clone, Debug, Default)]
pub struct StaticCredentials(pub Vec<(String, RoleSecret)>);

#[async_trait]
impl Credentials for StaticCredentials {
    async fn secret(&self, role: &str) -> Result<Option<RoleSecret>> {
        Ok(self
            .0
            .iter()
            .find(|(r, _)| r == role)
            .map(|(_, s)| s.clone()))
    }
}

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidSignature, msg)
}

/// The parsed `Authorization` header.
#[derive(Debug, PartialEq, Eq)]
struct AuthHeader {
    access_key: String,
    date: String,
    region: String,
    service: String,
    signed_headers: Vec<String>,
    signature: String,
}

fn parse_authorization(h: &str) -> Result<AuthHeader> {
    let h = h.trim();
    let (algo, rest) = h.split_once([' ', '\t']).unwrap_or((h, ""));
    if algo != "AWS4-HMAC-SHA256" {
        return Err(invalid(
            "Authorization header must use AWS4-HMAC-SHA256 algorithm",
        ));
    }
    let (mut credential, mut signed, mut signature) = (None, None, None);
    for part in rest.split([',', ' ', '\t']).filter(|p| !p.is_empty()) {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k {
            "Credential" => credential = Some(v),
            "SignedHeaders" => signed = Some(v),
            "Signature" => signature = Some(v),
            _ => {}
        }
    }
    let (Some(credential), Some(signed), Some(signature)) = (credential, signed, signature) else {
        return Err(invalid(format!(
            "Incorrect Authorization header format: {h}"
        )));
    };
    let scope: Vec<&str> = credential.split('/').collect();
    if scope.len() != 5 || scope[4] != "aws4_request" {
        return Err(invalid(format!(
            "Incorrect credential information format: {credential}"
        )));
    }
    Ok(AuthHeader {
        access_key: scope[0].to_string(),
        date: scope[1].to_string(),
        region: scope[2].to_string(),
        service: scope[3].to_string(),
        signed_headers: signed.split(';').map(|s| s.to_ascii_lowercase()).collect(),
        signature: signature.to_ascii_lowercase(),
    })
}

/// Collapses runs of whitespace and trims, as SigV4 canonicalisation requires.
fn canonical_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn canonical_query(q: &str) -> String {
    let mut pairs: Vec<(String, String)> = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k.to_string(), v.to_string())
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Computes the signature of `req` for `secret` (exposed for tests and clients).
pub fn sign(
    req: &HttpRequest,
    body: &[u8],
    signed_headers: &[String],
    amz_date: &str,
    scope: (&str, &str, &str),
    secret: &str,
) -> String {
    let (date, region, service) = scope;
    let mut canonical_headers = String::new();
    for name in signed_headers {
        let values: Vec<String> = req
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| canonical_value(v))
            .collect();
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(&values.join(","));
        canonical_headers.push('\n');
    }
    let payload_hash = hex::encode(Sha256::digest(body));
    let path = if req.path.is_empty() {
        "/"
    } else {
        req.path.as_str()
    };
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        uri_encode(path, true),
        canonical_query(&req.query),
        canonical_headers,
        signed_headers.join(";"),
        payload_hash
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{date}/{region}/{service}/aws4_request\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    hex::encode(hmac(&k_signing, string_to_sign.as_bytes()))
}

fn parse_amz_date(s: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|t| t.and_utc().timestamp())
}

/// The access key id a signed request names (its `Authorization` header's credential), if any.
pub fn access_key_id(req: &HttpRequest) -> Option<String> {
    parse_authorization(&req.header("authorization")?)
        .ok()
        .map(|a| a.access_key)
}

/// Verifies a request's signature; returns the authenticated role.
pub async fn verify(
    req: &HttpRequest,
    body: &[u8],
    creds: &dyn Credentials,
    max_skew_secs: i64,
) -> Result<String> {
    let Some(h) = req.header("authorization") else {
        return Err(Error::new(
            ErrorKind::MissingAuthenticationToken,
            "Authorization header is mandatory for signature verification",
        ));
    };
    let auth = parse_authorization(&h)?;
    let Some(amz_date) = req.header("x-amz-date") else {
        return Err(invalid(
            "X-Amz-Date header is mandatory for signature verification",
        ));
    };
    let Some(when) = parse_amz_date(amz_date.trim()) else {
        return Err(invalid(format!("Incorrect X-Amz-Date format: {amz_date}")));
    };
    let now = chrono::Utc::now().timestamp();
    let fmt = |t: i64| {
        chrono::DateTime::from_timestamp(t, 0)
            .map(|d| d.format("%Y%m%dT%H%M%SZ").to_string())
            .unwrap_or_default()
    };
    let minutes = max_skew_secs / 60;
    if when < now - max_skew_secs {
        return Err(invalid(format!(
            "Signature expired: {amz_date} is now earlier than {} ({} - {minutes} min.)",
            fmt(now - max_skew_secs),
            fmt(now)
        )));
    }
    if when > now + max_skew_secs {
        return Err(invalid(format!(
            "Signature not yet current: {amz_date} is still later than {} ({} + {minutes} min.)",
            fmt(now + max_skew_secs),
            fmt(now)
        )));
    }
    let Some(secret) = creds.secret(&auth.access_key).await? else {
        return Err(Error::new(
            ErrorKind::UnrecognizedClient,
            format!("User not found: {}", auth.access_key),
        ));
    };
    if !secret.can_login {
        return Err(Error::new(
            ErrorKind::UnrecognizedClient,
            format!("User {} has login=false", auth.access_key),
        ));
    }
    let expected = sign(
        req,
        body,
        &auth.signed_headers,
        amz_date.trim(),
        (&auth.date, &auth.region, &auth.service),
        &secret.salted_hash,
    );
    // ScyllaDB answers a wrong signature (a wrong secret) as an unrecognized client.
    if !constant_time_eq(expected.as_bytes(), auth.signature.as_bytes()) {
        return Err(Error::new(
            ErrorKind::UnrecognizedClient,
            "The request signature we calculated does not match the signature you provided.",
        ));
    }
    Ok(auth.access_key)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The AWS SigV4 test-suite example (`get-vanilla`), with its documented signature.
    #[test]
    fn aws_reference_vector() {
        let req = HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            headers: vec![
                ("Host".into(), "example.amazonaws.com".into()),
                ("X-Amz-Date".into(), "20150830T123600Z".into()),
            ],
            ..HttpRequest::default()
        };
        let sig = sign(
            &req,
            b"",
            &["host".into(), "x-amz-date".into()],
            "20150830T123600Z",
            ("20150830", "us-east-1", "service"),
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        );
        assert_eq!(
            sig,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn header_parsing() {
        let a = parse_authorization(
            "AWS4-HMAC-SHA256 Credential=role/20200101/us-east-1/dynamodb/aws4_request, SignedHeaders=Host;x-amz-date, Signature=ABC",
        )
        .unwrap();
        assert_eq!(a.access_key, "role");
        assert_eq!(a.signed_headers, ["host", "x-amz-date"]);
        assert_eq!(a.signature, "abc");
        // Commas or spaces between parts both work.
        assert!(
            parse_authorization(
                "AWS4-HMAC-SHA256 Credential=r/d/x/y/aws4_request SignedHeaders=host Signature=1"
            )
            .is_ok()
        );
        assert!(parse_authorization("AWS4-HMAC-SHA1 Credential=r/d/x/y/aws4_request").is_err());
        assert!(
            parse_authorization(
                "AWS4-HMAC-SHA256 Credential=r/d/aws4_request, SignedHeaders=host, Signature=1"
            )
            .is_err()
        );
        assert_eq!(canonical_value("  a   b  c "), "a b c");
    }
}
