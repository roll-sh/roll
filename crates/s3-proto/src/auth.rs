//! Which signature scheme a request uses, and verifying it whatever it is: SigV2
//! ([`crate::sigv2`]), SigV4 ([`crate::sigv4`]) or SigV4a ([`crate::sigv4a`]).

use crate::error::S3Error;
use crate::sigv4::{Request, Verified};
use crate::{sigv2, sigv4, sigv4a};
use p256::ecdsa::VerifyingKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    V2,
    V4,
    V4a,
}

/// The scheme a request is signed with, `None` for an anonymous request.
pub fn scheme(req: &Request) -> Option<Scheme> {
    if let Some(a) = req.header("authorization") {
        let a = a.trim();
        return Some(if a.starts_with("AWS ") {
            Scheme::V2
        } else if a.starts_with(sigv4a::ALGORITHM) {
            Scheme::V4a
        } else {
            Scheme::V4
        });
    }
    let query = crate::http::parse_query(req.query);
    let has = |name: &str| query.iter().any(|(k, _)| k == name);
    if query.iter().any(|(k, v)| k == "X-Amz-Algorithm" && v == sigv4a::ALGORITHM) {
        Some(Scheme::V4a)
    } else if has("X-Amz-Algorithm") || has("X-Amz-Signature") || has("X-Amz-Credential") {
        Some(Scheme::V4)
    } else if has("AWSAccessKeyId") || has("Signature") {
        Some(Scheme::V2)
    } else {
        None
    }
}

/// How to check a request beyond the request itself.
pub struct Context<'a> {
    /// Unix seconds now.
    pub now: i64,
    /// The bucket, when it came from the `Host` header (SigV2 signs it into the resource).
    pub vhost_bucket: Option<&'a str>,
}

/// Verify a request however it's signed: `Ok(None)` for anonymous requests. `lookup` maps an
/// access key to its secret; `ecdsa_key` gives an access key's SigV4a public key (cache
/// [`sigv4a::verifying_key`]).
pub fn authenticate(req: &Request, cx: &Context, lookup: impl FnOnce(&str) -> Option<String>, ecdsa_key: impl FnOnce(&str, &str) -> VerifyingKey) -> Result<Option<(Scheme, Verified)>, S3Error> {
    let Some(scheme) = scheme(req) else { return Ok(None) };
    let v = match scheme {
        Scheme::V2 => sigv2::authenticate(req, cx.now, cx.vhost_bucket, lookup)?,
        Scheme::V4 => sigv4::authenticate(req, cx.now, lookup)?,
        Scheme::V4a => sigv4a::authenticate(req, cx.now, lookup, ecdsa_key)?,
    };
    Ok(v.map(|v| (scheme, v)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes() {
        let s = |h: &[(&str, &str)], q: &str| scheme(&Request { method: "GET", path: "/", query: q, headers: h });
        assert_eq!(s(&[], ""), None);
        assert_eq!(s(&[], "prefix=a"), None);
        assert_eq!(s(&[("authorization", "AWS a:b")], ""), Some(Scheme::V2));
        assert_eq!(s(&[("authorization", "AWS4-HMAC-SHA256 Credential=x")], ""), Some(Scheme::V4));
        assert_eq!(s(&[("authorization", "AWS4-ECDSA-P256-SHA256 Credential=x")], ""), Some(Scheme::V4a));
        assert_eq!(s(&[("authorization", "Bearer x")], ""), Some(Scheme::V4), "SigV4's parser explains what's wrong");
        assert_eq!(s(&[], "X-Amz-Algorithm=AWS4-ECDSA-P256-SHA256"), Some(Scheme::V4a));
        assert_eq!(s(&[], "X-Amz-Algorithm=AWS4-HMAC-SHA256"), Some(Scheme::V4));
        assert_eq!(s(&[], "X-Amz-Signature=x"), Some(Scheme::V4));
        assert_eq!(s(&[], "AWSAccessKeyId=a&Expires=1&Signature=s"), Some(Scheme::V2));
        assert_eq!(s(&[], "Signature=s"), Some(Scheme::V2));
        let cx = Context { now: 0, vhost_bucket: None };
        assert_eq!(authenticate(&Request { method: "GET", path: "/", query: "", headers: &[] }, &cx, |_| None, sigv4a::verifying_key).unwrap(), None);
        let e = authenticate(&Request { method: "GET", path: "/", query: "Signature=s", headers: &[] }, &cx, |_| None, sigv4a::verifying_key).unwrap_err();
        assert_eq!(e.code, crate::Code::AccessDenied);
    }
}
