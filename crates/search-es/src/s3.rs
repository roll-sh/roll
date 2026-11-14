//! A small S3 client for buckets a tenant names (the `s3` snapshot repository,
//! [`crate::api::repository_s3`]): objects are read, written (optionally only when absent),
//! deleted and listed, with Signature Version 4 when the repository has credentials and
//! anonymously when it has none. roll's own credentials (its environment, instance role or
//! storage configuration) are never used for a tenant's bucket.
//!
//! The endpoint is the tenant's choice, so the client is built as AGENTS.md requires for any such
//! URL: names resolve to public addresses only ([`roll_net::PublicOnly`]), private IP literals
//! and `localhost` are refused ([`roll_net::literal_private_host`]), no redirect is followed, no
//! proxy is used, and every response body is read up to a bound the caller gives (error bodies
//! up to 64 KiB). Nothing it returns or logs carries a credential.

use bytes::Bytes;
use hkdf::hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Read at most this much of an error response.
const ERROR_BODY_MAX: usize = 64 << 10;
/// Attempts per request: S3 asks clients to retry 500 and 503.
const ATTEMPTS: u32 = 3;

/// Keys a request is signed with.
#[derive(Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials(..)")
    }
}

/// A failed request: the status and S3's error code and message when the bucket answered,
/// otherwise what went wrong on the way.
#[derive(Clone, Debug)]
pub struct Error {
    pub status: Option<u16>,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "{} ({s}): {}", self.code, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

fn err(code: &str, message: impl Into<String>) -> Error {
    Error { status: None, code: code.into(), message: message.into() }
}

/// Where a bucket is and how to reach it.
#[derive(Clone, Debug)]
pub struct Location {
    /// `scheme://host[:port]`, nothing else.
    pub endpoint: reqwest::Url,
    pub bucket: String,
    pub region: String,
    /// `https://host/bucket/key` instead of `https://bucket.host/key`.
    pub path_style: bool,
}

/// S3's bucket naming rules (3-63 lowercase letters, digits, dots and hyphens, starting and
/// ending with a letter or digit): a bucket goes into a host name or a path unescaped.
pub fn valid_bucket(b: &str) -> bool {
    let bytes = b.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'.' || *c == b'-')
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && !b.contains("..")
}

/// The endpoint a repository's `endpoint` and `protocol` settings name (AWS's regional
/// endpoint without one): `host[:port]` or a URL with no path, query or credentials. A private
/// address written out is refused unless `allow_private`.
pub fn endpoint(endpoint: Option<&str>, protocol: Option<&str>, region: &str, allow_private: bool) -> Result<reqwest::Url, String> {
    let scheme = match protocol.unwrap_or("https").to_ascii_lowercase().as_str() {
        "https" => "https",
        "http" => "http",
        other => return Err(format!("unsupported protocol [{other}]")),
    };
    let raw = match endpoint.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) if e.contains("://") => e.to_string(),
        Some(e) => format!("{scheme}://{e}"),
        None => format!("https://s3.{region}.amazonaws.com"),
    };
    let url = reqwest::Url::parse(&raw).map_err(|_| format!("invalid endpoint [{raw}]"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(format!("invalid endpoint [{raw}]"));
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
        return Err(format!("endpoint [{raw}] must be a scheme, host and port only"));
    }
    if !allow_private && roll_net::literal_private_host(&url) {
        return Err(format!("endpoint [{raw}] is a private address"));
    }
    let mut base = url.clone();
    base.set_path("");
    Ok(base)
}

/// An S3 bucket.
pub struct Client {
    http: reqwest::Client,
    at: Location,
    creds: Option<Credentials>,
}

impl Client {
    /// A client for `at`. Private addresses are refused (as literals here, resolved ones at
    /// connect time) unless `allow_private`.
    pub fn new(at: Location, creds: Option<Credentials>, allow_private: bool) -> Result<Client, Error> {
        if !valid_bucket(&at.bucket) {
            return Err(err("InvalidBucketName", format!("invalid bucket name [{}]", at.bucket)));
        }
        if !allow_private && roll_net::literal_private_host(&at.endpoint) {
            return Err(err("PrivateAddress", "the endpoint is a private address"));
        }
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .timeout(Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if !allow_private {
            b = b.dns_resolver(std::sync::Arc::new(roll_net::PublicOnly));
        }
        let http = b.build().map_err(|e| err("ClientError", e.to_string()))?;
        Ok(Client { http, at, creds })
    }

    /// The URL of `key` (already a valid object key; empty for the bucket itself) and its
    /// canonical (signed) path.
    fn url(&self, key: &str, query: &[(&str, &str)]) -> (reqwest::Url, String) {
        let path = match self.at.path_style {
            true => format!("/{}/{}", self.at.bucket, uri_encode(key, false)),
            false => format!("/{}", uri_encode(key, false)),
        };
        let mut u = self.at.endpoint.clone();
        if !self.at.path_style {
            let host = format!("{}.{}", self.at.bucket, self.at.endpoint.host_str().unwrap_or_default());
            let _ = u.set_host(Some(&host));
        }
        let q = canonical_query(query);
        let full = format!("{}://{}{path}{}{q}", u.scheme(), authority(&u), if q.is_empty() { "" } else { "?" });
        (reqwest::Url::parse(&full).unwrap_or(u), path)
    }

    async fn send(&self, method: reqwest::Method, key: &str, query: &[(&str, &str)], extra: &[(&str, &str)], body: Bytes) -> Result<reqwest::Response, Error> {
        let (url, path) = self.url(key, query);
        let payload = hex(&Sha256::digest(&body));
        let mut last = err("RequestFailed", "no attempt was made");
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
            }
            let now = chrono::Utc::now();
            let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
            let mut headers: Vec<(String, String)> = vec![
                ("host".into(), authority(&url)),
                ("x-amz-content-sha256".into(), payload.clone()),
                ("x-amz-date".into(), amz_date.clone()),
            ];
            for (k, v) in extra {
                headers.push((k.to_ascii_lowercase(), v.to_string()));
            }
            if let Some(c) = &self.creds {
                if let Some(t) = &c.session_token {
                    headers.push(("x-amz-security-token".into(), t.clone()));
                }
                let auth = authorization(c, &self.at.region, method.as_str(), &path, &canonical_query(query), &mut headers, &payload, &amz_date);
                headers.push(("authorization".into(), auth));
            }
            let mut rb = self.http.request(method.clone(), url.clone());
            for (k, v) in &headers {
                if k != "host" {
                    rb = rb.header(k.as_str(), v.as_str());
                }
            }
            match rb.body(body.clone()).send().await {
                Ok(r) if matches!(r.status().as_u16(), 500 | 502 | 503 | 504) && attempt + 1 < ATTEMPTS => last = failure(r).await,
                Ok(r) => return Ok(r),
                Err(e) if attempt + 1 < ATTEMPTS && !e.is_builder() => last = err("RequestFailed", transport(&e)),
                Err(e) => return Err(err("RequestFailed", transport(&e))),
            }
        }
        Err(last)
    }

    /// The object at `key`, or `None` when there is none. More than `max` bytes is an error.
    pub async fn get(&self, key: &str, max: usize) -> Result<Option<Bytes>, Error> {
        let r = self.send(reqwest::Method::GET, key, &[], &[], Bytes::new()).await?;
        match r.status().as_u16() {
            200 => read_bounded(r, max).await.map(Some),
            404 => Ok(None),
            _ => Err(failure(r).await),
        }
    }

    /// Writes the object at `key`.
    pub async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let r = self.send(reqwest::Method::PUT, key, &[], &[], body).await?;
        match r.status().is_success() {
            true => Ok(()),
            false => Err(failure(r).await),
        }
    }

    /// Writes the object at `key` only when there is none (`If-None-Match: *`): `false` when
    /// one exists.
    pub async fn put_new(&self, key: &str, body: Bytes) -> Result<bool, Error> {
        let r = self.send(reqwest::Method::PUT, key, &[], &[("If-None-Match", "*")], body).await?;
        match r.status().as_u16() {
            200..=299 => Ok(true),
            // 409: a conditional write raced another one on the same key.
            412 | 409 => Ok(false),
            _ => Err(failure(r).await),
        }
    }

    /// Deletes the object at `key` (none there is fine).
    pub async fn delete(&self, key: &str) -> Result<(), Error> {
        let r = self.send(reqwest::Method::DELETE, key, &[], &[], Bytes::new()).await?;
        match r.status().as_u16() {
            200..=299 | 404 => Ok(()),
            _ => Err(failure(r).await),
        }
    }

    /// Every object under `prefix`, as `(key, size)`.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, Error> {
        const PAGE_MAX: usize = 16 << 20;
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(t) = &token {
                q.push(("continuation-token", t.as_str()));
            }
            let r = self.send(reqwest::Method::GET, "", &q, &[], Bytes::new()).await?;
            if !r.status().is_success() {
                return Err(failure(r).await);
            }
            let page = read_bounded(r, PAGE_MAX).await?;
            let page = String::from_utf8_lossy(&page);
            for item in elements(&page, "Contents") {
                let key = elements(item, "Key").next().map(xml_unescape).unwrap_or_default();
                let size = elements(item, "Size").next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                out.push((key, size));
            }
            let truncated = elements(&page, "IsTruncated").next().is_some_and(|t| t.trim() == "true");
            token = elements(&page, "NextContinuationToken").next().map(xml_unescape);
            if !truncated || token.is_none() {
                return Ok(out);
            }
        }
    }
}

/// `host[:port]` of a URL as the `Host` header carries it.
fn authority(u: &reqwest::Url) -> String {
    match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
        None => u.host_str().unwrap_or_default().to_string(),
    }
}

fn transport(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(x) = src {
        s = format!("{s}: {x}");
        src = x.source();
    }
    s
}

async fn read_bounded(mut r: reqwest::Response, max: usize) -> Result<Bytes, Error> {
    if r.content_length().is_some_and(|n| n > max as u64) {
        return Err(err("TooLarge", format!("the object is larger than {max} bytes")));
    }
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await.map_err(|e| err("RequestFailed", transport(&e)))? {
        if out.len() + chunk.len() > max {
            return Err(err("TooLarge", format!("the object is larger than {max} bytes")));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(out))
}

/// An error response: S3's `Code` and `Message`, from at most [`ERROR_BODY_MAX`] bytes.
async fn failure(r: reqwest::Response) -> Error {
    let status = r.status().as_u16();
    let body = read_bounded(r, ERROR_BODY_MAX).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&body);
    let code = elements(&text, "Code").next().map(xml_unescape).unwrap_or_else(|| format!("Http{status}"));
    let message = elements(&text, "Message").next().map(xml_unescape).unwrap_or_default();
    Error { status: Some(status), code, message }
}

/// The contents of each `<tag>…</tag>` in `xml` (S3's responses have no attributes on these).
fn elements<'a>(xml: &'a str, tag: &str) -> impl Iterator<Item = &'a str> + 'a {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut rest = xml;
    std::iter::from_fn(move || {
        let start = rest.find(&open)? + open.len();
        let end = start + rest[start..].find(&close)?;
        let v = &rest[start..end];
        rest = &rest[end + close.len()..];
        Some(v)
    })
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

pub(crate) fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

/// SigV4's URI encoding: unreserved characters kept, `/` kept unless `slash`, the rest `%XX`.
fn uri_encode(s: &str, slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if !slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn canonical_query(q: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = q.iter().map(|(k, v)| (uri_encode(k, true), uri_encode(v, true))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

fn hmac(key: &[u8], data: &str) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().into()
}

/// The `Authorization` header of a request (SigV4, service `s3`): `headers` (lowercase names)
/// are all signed.
#[allow(clippy::too_many_arguments)]
fn authorization(c: &Credentials, region: &str, method: &str, path: &str, query: &str, headers: &mut [(String, String)], payload: &str, amz_date: &str) -> String {
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed = signed.join(";");
    let request = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&Sha256::digest(request.as_bytes())));
    let mut key = hmac(format!("AWS4{}", c.secret_key).as_bytes(), date);
    for part in [region, "s3", "aws4_request"] {
        key = hmac(&key, part);
    }
    let signature = hex(&hmac(&key, &to_sign));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", c.access_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's worked example for SigV4 with headers ("Example: GET Object" in the S3 API
    /// reference's "Signature Calculations for the Authorization Header").
    #[test]
    fn signs_as_aws_documents() {
        let c = Credentials { access_key: "AKIAIOSFODNN7EXAMPLE".into(), secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), session_token: None };
        let empty = hex(&Sha256::digest(b""));
        let mut headers = vec![
            ("host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), empty.clone()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let auth = authorization(&c, "us-east-1", "GET", "/test.txt", "", &mut headers, &empty, "20130524T000000Z");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn endpoints_tenants_may_name() {
        assert_eq!(endpoint(None, None, "eu-west-1", false).unwrap().as_str(), "https://s3.eu-west-1.amazonaws.com/");
        assert_eq!(endpoint(Some("minio.example.com:9000"), Some("http"), "us-east-1", false).unwrap().as_str(), "http://minio.example.com:9000/");
        for private in ["127.0.0.1:9000", "http://10.0.0.5", "localhost", "[::1]:80", "169.254.169.254"] {
            assert!(endpoint(Some(private), None, "us-east-1", false).is_err(), "{private}");
        }
        assert!(endpoint(Some("127.0.0.1:9000"), Some("http"), "us-east-1", true).is_ok());
        assert!(endpoint(Some("https://user:pw@s3.example.com"), None, "us-east-1", false).is_err());
        assert!(endpoint(Some("https://s3.example.com/elsewhere"), None, "us-east-1", false).is_err());
        assert!(valid_bucket("my-bucket.1") && !valid_bucket("evil.com/x") && !valid_bucket("UP") && !valid_bucket("a..b"));
    }
}
