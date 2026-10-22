//! AWS Signature Version 4 for S3: the `Authorization` header, presigned URLs, presigned POST
//! policies, and signing (presigned URLs for the control plane, requests and chunks for tests).
//!
//! The server parses with [`parse`], checks the clock with [`check_time`], looks up the secret,
//! derives (and may cache) the key with [`signing_key`], then [`verify`]s. For streaming
//! payloads the result carries what [`crate::chunked::Decoder`] needs.

use crate::error::{Code, S3Error};
use crate::http::{percent_decode, uri_encode, uri_encode_bytes};
use crate::time;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
pub const STREAMING_PAYLOAD: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
pub const STREAMING_PAYLOAD_TRAILER: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER";
pub const STREAMING_UNSIGNED_TRAILER: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";
/// SHA-256 of nothing.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// Allowed clock difference for header-signed requests.
pub const MAX_SKEW_S: i64 = 15 * 60;
/// Longest presigned URL lifetime.
pub const MAX_PRESIGN_S: i64 = 7 * 24 * 3600;

/// A request as the server received it.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub method: &'a str,
    /// The path as on the request line (percent-encoded).
    pub path: &'a str,
    /// The raw query string, without `?`.
    pub query: &'a str,
    /// Every header, any case, repeated names allowed.
    pub headers: &'a [(&'a str, &'a str)],
}

impl<'a> Request<'a> {
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| *v)
    }
}

/// What the payload hash says about the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    /// `UNSIGNED-PAYLOAD`.
    Unsigned,
    /// A hex SHA-256 of the body, to check once it's read.
    Sha256(String),
    /// `aws-chunked`, every chunk signed; `trailer`: with signed trailers.
    Streaming { trailer: bool },
    /// `aws-chunked`, unsigned chunks, trailers.
    StreamingUnsignedTrailer,
}

impl Payload {
    pub fn parse(v: &str) -> Result<Self, S3Error> {
        Ok(match v.trim() {
            UNSIGNED_PAYLOAD => Payload::Unsigned,
            STREAMING_PAYLOAD => Payload::Streaming { trailer: false },
            STREAMING_PAYLOAD_TRAILER => Payload::Streaming { trailer: true },
            STREAMING_UNSIGNED_TRAILER => Payload::StreamingUnsignedTrailer,
            s if s.starts_with("STREAMING-AWS4-ECDSA") => return Err(S3Error::msg(Code::NotImplemented, "SigV4a is not supported").with("Header", "x-amz-content-sha256")),
            s if s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()) => Payload::Sha256(s.to_ascii_lowercase()),
            _ => return Err(S3Error::msg(Code::InvalidArgument, "x-amz-content-sha256 must be UNSIGNED-PAYLOAD, STREAMING-AWS4-HMAC-SHA256-PAYLOAD, STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER, STREAMING-UNSIGNED-PAYLOAD-TRAILER or a valid sha256 value.").with("ArgumentName", "x-amz-content-sha256")),
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            Payload::Unsigned => UNSIGNED_PAYLOAD,
            Payload::Sha256(h) => h,
            Payload::Streaming { trailer: false } => STREAMING_PAYLOAD,
            Payload::Streaming { trailer: true } => STREAMING_PAYLOAD_TRAILER,
            Payload::StreamingUnsignedTrailer => STREAMING_UNSIGNED_TRAILER,
        }
    }

    /// The body is `aws-chunked`.
    pub fn is_chunked(&self) -> bool {
        matches!(self, Payload::Streaming { .. } | Payload::StreamingUnsignedTrailer)
    }
}

/// `<access key>/<yyyymmdd>/<region>/<service>/aws4_request`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub access_key: String,
    pub date: String,
    pub region: String,
    pub service: String,
}

impl Credential {
    pub fn parse(s: &str, code: Code) -> Result<Self, S3Error> {
        let parts: Vec<&str> = s.trim().split('/').collect();
        let bad = |why: &str| S3Error::msg(code, format!("Error parsing the X-Amz-Credential parameter; {why}"));
        if parts.len() != 5 || parts[4] != "aws4_request" {
            return Err(bad("the Credential is mal-formed; expecting \"<YOUR-AKID>/YYYYMMDD/REGION/SERVICE/aws4_request\"."));
        }
        if parts[0].is_empty() {
            return Err(bad("the access key is empty."));
        }
        if parts[1].len() != 8 || !parts[1].bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad(&format!("incorrect date format \"{}\". This date in the credential must be in the format \"yyyyMMdd\".", parts[1])));
        }
        if parts[3] != "s3" {
            return Err(bad(&format!("incorrect service \"{}\". This endpoint belongs to \"s3\".", parts[3])));
        }
        Ok(Self { access_key: parts[0].into(), date: parts[1].into(), region: parts[2].into(), service: parts[3].into() })
    }

    pub fn scope(&self) -> String {
        format!("{}/{}/{}/aws4_request", self.date, self.region, self.service)
    }
}

/// A signed request, parsed (not yet verified).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub credential: Credential,
    /// Lowercase, as listed.
    pub signed_headers: Vec<String>,
    pub signature: String,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: String,
    /// The same as Unix seconds.
    pub timestamp: i64,
    pub payload: Payload,
    /// Presigned URLs: `X-Amz-Expires` in seconds.
    pub presigned_expires: Option<i64>,
}

/// What a verified request leaves for reading the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub access_key: String,
    pub payload: Payload,
    /// For signed `aws-chunked` bodies.
    pub stream: Option<StreamSigning>,
}

/// The chunk-signing context: the seed (request) signature and its key and scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSigning {
    pub signing_key: [u8; 32],
    pub seed_signature: String,
    pub amz_date: String,
    pub scope: String,
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// The SigV4 signing key for a day, region and service. Constant for a whole day, so worth
/// caching per `(access key, date, region)`.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

/// Hex HMAC-SHA256 of `data` with a signing key.
pub fn sign(signing_key: &[u8; 32], data: &str) -> String {
    hex::encode(hmac(signing_key, data.as_bytes()))
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn malformed(why: impl Into<String>) -> S3Error {
    S3Error::msg(Code::AuthorizationHeaderMalformed, why)
}

/// Parse how a request is signed: `None` for anonymous requests. Doesn't check the clock or
/// the signature.
pub fn parse(req: &Request) -> Result<Option<Signed>, S3Error> {
    if let Some(auth) = req.header("authorization") {
        return parse_header(req, auth).map(Some);
    }
    let query = crate::http::parse_query(req.query);
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if q("X-Amz-Algorithm").is_some() || q("X-Amz-Signature").is_some() || q("X-Amz-Credential").is_some() {
        return parse_presigned(&query).map(Some);
    }
    if q("Signature").is_some() && q("AWSAccessKeyId").is_some() {
        return Err(S3Error::msg(Code::InvalidRequest, "Signature Version 2 is not supported; use AWS4-HMAC-SHA256."));
    }
    Ok(None)
}

fn parse_header(req: &Request, auth: &str) -> Result<Signed, S3Error> {
    let auth = auth.trim();
    if auth.starts_with("AWS ") {
        return Err(S3Error::msg(Code::InvalidRequest, "Signature Version 2 is not supported; use AWS4-HMAC-SHA256."));
    }
    let rest = auth.strip_prefix(ALGORITHM).ok_or_else(|| malformed("Unsupported authorization type"))?;
    let (mut cred, mut signed_headers, mut signature) = (None, None, None);
    for part in rest.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=').ok_or_else(|| malformed(format!("The authorization header is malformed; incorrect component \"{part}\".")))?;
        match k.trim() {
            "Credential" => cred = Some(v.trim()),
            "SignedHeaders" => signed_headers = Some(v.trim()),
            "Signature" => signature = Some(v.trim()),
            _ => {}
        }
    }
    let credential = Credential::parse(cred.ok_or_else(|| malformed("The authorization header is malformed; missing Credential."))?, Code::AuthorizationHeaderMalformed)?;
    let signed_headers: Vec<String> = signed_headers.ok_or_else(|| malformed("The authorization header is malformed; missing SignedHeaders."))?.split(';').map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()).collect();
    let signature = signature.ok_or_else(|| malformed("The authorization header is malformed; missing Signature."))?.to_ascii_lowercase();
    if !signed_headers.iter().any(|h| h == "host") {
        return Err(S3Error::msg(Code::AccessDenied, "There were headers present in the request which were not signed").with("HeadersNotSigned", "host"));
    }
    let (amz_date, timestamp) = match req.header("x-amz-date") {
        Some(d) => (d.trim().to_string(), time::parse_amz_date(d.trim()).ok_or_else(|| S3Error::msg(Code::AccessDenied, "AWS authentication requires a valid Date or x-amz-date header"))?),
        None => {
            let d = req.header("date").ok_or_else(|| S3Error::msg(Code::AccessDenied, "AWS authentication requires a valid Date or x-amz-date header"))?;
            let t = time::parse_http_date(d).ok_or_else(|| S3Error::msg(Code::AccessDenied, "AWS authentication requires a valid Date or x-amz-date header"))?;
            (time::amz_date(t), t)
        }
    };
    if amz_date[..8] != credential.date {
        return Err(malformed(format!("The authorization header is malformed; Invalid credential date \"{}\". This date is not the same as X-Amz-Date: \"{}\".", credential.date, &amz_date[..8])));
    }
    let payload = Payload::parse(req.header("x-amz-content-sha256").ok_or_else(|| S3Error::msg(Code::InvalidRequest, "Missing required header for this request: x-amz-content-sha256"))?)?;
    Ok(Signed { credential, signed_headers, signature, amz_date, timestamp, payload, presigned_expires: None })
}

fn parse_presigned(query: &[(String, String)]) -> Result<Signed, S3Error> {
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let missing = || S3Error::msg(Code::AuthorizationQueryParametersError, "Query-string authentication version 4 requires the X-Amz-Algorithm, X-Amz-Credential, X-Amz-Signature, X-Amz-Date, X-Amz-SignedHeaders, and X-Amz-Expires parameters.");
    if q("X-Amz-Algorithm").ok_or_else(missing)? != ALGORITHM {
        return Err(S3Error::msg(Code::AuthorizationQueryParametersError, "X-Amz-Algorithm only supports \"AWS4-HMAC-SHA256\""));
    }
    let credential = Credential::parse(q("X-Amz-Credential").ok_or_else(missing)?, Code::AuthorizationQueryParametersError)?;
    let amz_date = q("X-Amz-Date").ok_or_else(missing)?.to_string();
    let timestamp = time::parse_amz_date(&amz_date).ok_or_else(|| S3Error::msg(Code::AuthorizationQueryParametersError, "X-Amz-Date must be in the ISO8601 Long Format \"yyyyMMdd'T'HHmmss'Z'\""))?;
    if amz_date[..8] != credential.date {
        return Err(S3Error::msg(Code::AuthorizationQueryParametersError, "Invalid credential date. This date is not the same as X-Amz-Date."));
    }
    let expires: i64 = q("X-Amz-Expires").ok_or_else(missing)?.parse().map_err(|_| S3Error::msg(Code::AuthorizationQueryParametersError, "X-Amz-Expires should be a number"))?;
    if expires < 1 {
        return Err(S3Error::msg(Code::AuthorizationQueryParametersError, "X-Amz-Expires must be non-negative"));
    }
    if expires > MAX_PRESIGN_S {
        return Err(S3Error::msg(Code::AuthorizationQueryParametersError, "X-Amz-Expires must be less than a week (in seconds) that is 604800"));
    }
    let signed_headers: Vec<String> = q("X-Amz-SignedHeaders").ok_or_else(missing)?.split(';').map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()).collect();
    if !signed_headers.iter().any(|h| h == "host") {
        return Err(S3Error::msg(Code::AccessDenied, "There were headers present in the request which were not signed").with("HeadersNotSigned", "host"));
    }
    let signature = q("X-Amz-Signature").ok_or_else(missing)?.to_ascii_lowercase();
    let payload = match q("X-Amz-Content-Sha256") {
        Some(v) => Payload::parse(v)?,
        None => Payload::Unsigned,
    };
    if payload.is_chunked() {
        return Err(S3Error::msg(Code::InvalidRequest, "Streaming payloads can't be presigned"));
    }
    Ok(Signed { credential, signed_headers, signature, amz_date, timestamp, payload, presigned_expires: Some(expires) })
}

/// Clock checks: header-signed requests within 15 minutes of `now`, presigned URLs not yet
/// expired (and not dated in the future).
pub fn check_time(s: &Signed, now: i64) -> Result<(), S3Error> {
    match s.presigned_expires {
        Some(expires) => {
            if s.timestamp > now + MAX_SKEW_S {
                return Err(S3Error::msg(Code::AccessDenied, "Request is not valid yet"));
            }
            if now > s.timestamp + expires {
                return Err(S3Error::msg(Code::AccessDenied, "Request has expired").with("X-Amz-Expires", expires.to_string()).with("Expires", time::iso8601_ms((s.timestamp + expires) * 1000)).with("ServerTime", time::iso8601_ms(now * 1000)));
            }
        }
        None => {
            if (s.timestamp - now).abs() > MAX_SKEW_S {
                return Err(S3Error::new(Code::RequestTimeTooSkewed).with("RequestTime", s.amz_date.clone()).with("ServerTime", time::iso8601_ms(now * 1000)).with("MaxAllowedSkewMilliseconds", (MAX_SKEW_S * 1000).to_string()));
            }
        }
    }
    Ok(())
}

/// The canonical URI: the path decoded once and encoded once by SigV4's rules (S3 doesn't
/// normalise paths).
pub fn canonical_uri(raw_path: &str) -> String {
    if raw_path.is_empty() {
        return "/".into();
    }
    uri_encode_bytes(&percent_decode(raw_path), false)
}

/// The canonical query string; presigned requests leave out `X-Amz-Signature`.
pub fn canonical_query(raw_query: &str, presigned: bool) -> String {
    let mut pairs: Vec<(String, String)> = raw_query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            let k = uri_encode_bytes(&percent_decode(k), true);
            if presigned && k == "X-Amz-Signature" {
                return None;
            }
            Some((k, uri_encode_bytes(&percent_decode(v), true)))
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

fn canonical_header_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The canonical request of a parsed request.
pub fn canonical_request(req: &Request, s: &Signed) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(req.method);
    out.push('\n');
    out.push_str(&canonical_uri(req.path));
    out.push('\n');
    out.push_str(&canonical_query(req.query, s.presigned_expires.is_some()));
    out.push('\n');
    for name in &s.signed_headers {
        let values: Vec<String> = req.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| canonical_header_value(v)).collect();
        out.push_str(name);
        out.push(':');
        out.push_str(&values.join(","));
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&s.signed_headers.join(";"));
    out.push('\n');
    out.push_str(s.payload.as_str());
    out
}

pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!("{ALGORITHM}\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical_request.as_bytes())))
}

/// Check the signature with the key from [`signing_key`].
pub fn verify(req: &Request, s: &Signed, signing_key: &[u8; 32]) -> Result<Verified, S3Error> {
    for h in &s.signed_headers {
        if req.header(h).is_none() {
            return Err(S3Error::msg(Code::SignatureDoesNotMatch, format!("Signed header {h} is missing from the request")));
        }
    }
    let canonical = canonical_request(req, s);
    let sts = string_to_sign(&s.amz_date, &s.credential.scope(), &canonical);
    let expected = sign(signing_key, &sts);
    if !constant_time_eq(&expected, &s.signature) {
        return Err(S3Error::new(Code::SignatureDoesNotMatch)
            .with("AWSAccessKeyId", s.credential.access_key.clone())
            .with("StringToSign", sts)
            .with("SignatureProvided", s.signature.clone())
            .with("CanonicalRequest", canonical));
    }
    let stream = matches!(s.payload, Payload::Streaming { .. }).then(|| StreamSigning { signing_key: *signing_key, seed_signature: s.signature.clone(), amz_date: s.amz_date.clone(), scope: s.credential.scope() });
    Ok(Verified { access_key: s.credential.access_key.clone(), payload: s.payload.clone(), stream })
}

/// Everything in one call: `Ok(None)` for anonymous requests; `lookup` maps an access key to
/// its secret (`InvalidAccessKeyId` if it returns `None`).
pub fn authenticate(req: &Request, now: i64, lookup: impl FnOnce(&str) -> Option<String>) -> Result<Option<Verified>, S3Error> {
    let Some(s) = parse(req)? else { return Ok(None) };
    let secret = lookup(&s.credential.access_key).ok_or_else(|| S3Error::new(Code::InvalidAccessKeyId).with("AWSAccessKeyId", s.credential.access_key.clone()))?;
    check_time(&s, now)?;
    let key = signing_key(&secret, &s.credential.date, &s.credential.region, &s.credential.service);
    verify(req, &s, &key).map(Some)
}

/// `scheme://host[:port]` → `host[:port]` (what goes in the `host` header).
fn endpoint_host(endpoint: &str) -> &str {
    let rest = endpoint.split_once("://").map_or(endpoint, |(_, r)| r);
    let host = rest.split('/').next().unwrap_or(rest);
    if let Some(h) = host.strip_suffix(":80").filter(|_| endpoint.starts_with("http://")) {
        return h;
    }
    if let Some(h) = host.strip_suffix(":443").filter(|_| endpoint.starts_with("https://")) {
        return h;
    }
    host
}

/// A presigned URL request.
#[derive(Clone, Copy, Debug)]
pub struct Presign<'a> {
    pub method: &'a str,
    /// `http://127.0.0.1:59000`
    pub endpoint: &'a str,
    /// Decoded path, e.g. `/bucket/my key.txt`.
    pub path: &'a str,
    /// Extra query parameters (decoded), e.g. `response-content-disposition`.
    pub query: &'a [(&'a str, &'a str)],
    pub access_key: &'a str,
    pub secret: &'a str,
    pub region: &'a str,
    /// Unix seconds.
    pub now: i64,
    pub expires_s: i64,
}

/// A presigned URL (host-only signed headers, unsigned payload).
pub fn presign_url(p: &Presign) -> String {
    let amz_date = time::amz_date(p.now);
    let date = &amz_date[..8];
    let credential = format!("{}/{date}/{}/s3/aws4_request", p.access_key, p.region);
    let mut params: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".into(), ALGORITHM.into()),
        ("X-Amz-Credential".into(), credential),
        ("X-Amz-Date".into(), amz_date.clone()),
        ("X-Amz-Expires".into(), p.expires_s.clamp(1, MAX_PRESIGN_S).to_string()),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    params.extend(p.query.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let query = params.iter().map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true))).collect::<Vec<_>>().join("&");
    let path = uri_encode(p.path, false);
    let host = endpoint_host(p.endpoint);
    let headers = [("host", host)];
    let req = Request { method: p.method, path: &path, query: &query, headers: &headers };
    let s = Signed {
        credential: Credential { access_key: p.access_key.into(), date: date.into(), region: p.region.into(), service: "s3".into() },
        signed_headers: vec!["host".into()],
        signature: String::new(),
        amz_date: amz_date.clone(),
        timestamp: p.now,
        payload: Payload::Unsigned,
        presigned_expires: Some(p.expires_s),
    };
    let sts = string_to_sign(&amz_date, &s.credential.scope(), &canonical_request(&req, &s));
    let sig = sign(&signing_key(p.secret, date, p.region, "s3"), &sts);
    format!("{}{path}?{query}&X-Amz-Signature={sig}", p.endpoint.trim_end_matches('/'))
}

/// Sign a request (clients and tests): every given header is signed, and must include `host`,
/// `x-amz-date` and `x-amz-content-sha256`. Returns the `Authorization` value.
pub fn authorization(method: &str, raw_path: &str, raw_query: &str, headers: &[(&str, &str)], access_key: &str, secret: &str, region: &str) -> String {
    let req = Request { method, path: raw_path, query: raw_query, headers };
    let amz_date = req.header("x-amz-date").unwrap_or_default().to_string();
    let date = amz_date.get(..8).unwrap_or_default().to_string();
    let mut names: Vec<String> = headers.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
    names.sort();
    names.dedup();
    let payload = req.header("x-amz-content-sha256").and_then(|v| Payload::parse(v).ok()).unwrap_or(Payload::Unsigned);
    let s = Signed {
        credential: Credential { access_key: access_key.into(), date: date.clone(), region: region.into(), service: "s3".into() },
        signed_headers: names.clone(),
        signature: String::new(),
        amz_date: amz_date.clone(),
        timestamp: 0,
        payload,
        presigned_expires: None,
    };
    let sts = string_to_sign(&amz_date, &s.credential.scope(), &canonical_request(&req, &s));
    let sig = sign(&signing_key(secret, &date, region, "s3"), &sts);
    format!("{ALGORITHM} Credential={access_key}/{}, SignedHeaders={}, Signature={sig}", s.credential.scope(), names.join(";"))
}

/// The signature of one `aws-chunked` chunk.
pub fn chunk_signature(key: &[u8; 32], amz_date: &str, scope: &str, previous: &str, chunk_sha256_hex: &str) -> String {
    sign(key, &format!("AWS4-HMAC-SHA256-PAYLOAD\n{amz_date}\n{scope}\n{previous}\n{EMPTY_SHA256}\n{chunk_sha256_hex}"))
}

/// The signature of the trailers (`name:value\n` each, in order) after the final chunk.
pub fn trailer_signature(key: &[u8; 32], amz_date: &str, scope: &str, previous: &str, trailers: &[(String, String)]) -> String {
    let mut canon = String::new();
    for (k, v) in trailers {
        canon.push_str(k);
        canon.push(':');
        canon.push_str(v);
        canon.push('\n');
    }
    sign(key, &format!("AWS4-HMAC-SHA256-TRAILER\n{amz_date}\n{scope}\n{previous}\n{}", hex::encode(Sha256::digest(canon.as_bytes()))))
}

/// Credentials of a presigned POST form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostAuth {
    pub credential: Credential,
    pub amz_date: String,
    /// The base64 policy, as sent.
    pub policy: String,
    pub signature: String,
}

/// What a POST policy allows beyond the form fields it checked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PostPolicy {
    /// `content-length-range`: check the file size against it.
    pub content_length_range: Option<(u64, u64)>,
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// The signature fields of a POST form; `None` if it has no `policy` (anonymous upload).
pub fn parse_post_auth(fields: &[(String, String)]) -> Result<Option<PostAuth>, S3Error> {
    let Some(policy) = field(fields, "policy") else { return Ok(None) };
    let bad = |what: &str| S3Error::msg(Code::InvalidArgument, format!("Bucket POST must contain a field named '{what}'.  If it is specified, please check the order of the fields.")).with("ArgumentName", what.to_string());
    if field(fields, "x-amz-algorithm").ok_or_else(|| bad("x-amz-algorithm"))? != ALGORITHM {
        return Err(S3Error::msg(Code::InvalidArgument, "Only AWS4-HMAC-SHA256 is supported for POST").with("ArgumentName", "x-amz-algorithm"));
    }
    let credential = Credential::parse(field(fields, "x-amz-credential").ok_or_else(|| bad("x-amz-credential"))?, Code::InvalidArgument)?;
    let amz_date = field(fields, "x-amz-date").ok_or_else(|| bad("x-amz-date"))?.to_string();
    let signature = field(fields, "x-amz-signature").ok_or_else(|| bad("x-amz-signature"))?.to_ascii_lowercase();
    Ok(Some(PostAuth { credential, amz_date, policy: policy.to_string(), signature }))
}

/// Check a POST form's signature (HMAC of the base64 policy).
pub fn verify_post_signature(auth: &PostAuth, signing_key: &[u8; 32]) -> Result<(), S3Error> {
    if constant_time_eq(&sign(signing_key, &auth.policy), &auth.signature) {
        Ok(())
    } else {
        Err(S3Error::new(Code::SignatureDoesNotMatch).with("AWSAccessKeyId", auth.credential.access_key.clone()).with("StringToSign", auth.policy.clone()))
    }
}

/// Fields that never need a policy condition.
fn exempt(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "x-amz-signature" || n == "file" || n == "policy" || n.starts_with("x-ignore-")
}

/// Check a POST policy (base64 JSON) against the form's fields (`key` with `${filename}`
/// already substituted) at `now_ms`: not expired, every condition holds, and every field has
/// a condition. The caller checks the file size against `content_length_range`.
pub fn check_post_policy(policy_b64: &str, fields: &[(String, String)], bucket: &str, now_ms: i64) -> Result<PostPolicy, S3Error> {
    let invalid = |why: String| S3Error::msg(Code::InvalidPolicyDocument, why);
    let raw = crate::checksum::b64_decode(policy_b64).ok_or_else(|| invalid("Invalid Policy: Invalid JSON.".into()))?;
    let doc: serde_json::Value = serde_json::from_slice(&raw).map_err(|_| invalid("Invalid Policy: Invalid JSON.".into()))?;
    let expiration = doc.get("expiration").and_then(|v| v.as_str()).ok_or_else(|| invalid("Invalid Policy: Policy missing expiration.".into()))?;
    let exp_ms = time::parse_iso8601(expiration).ok_or_else(|| invalid("Invalid Policy: Invalid 'expiration' value.".into()))?;
    if now_ms > exp_ms {
        return Err(S3Error::msg(Code::AccessDenied, "Invalid according to Policy: Policy expired."));
    }
    let conditions = doc.get("conditions").and_then(|v| v.as_array()).ok_or_else(|| invalid("Invalid Policy: Policy missing conditions.".into()))?;
    let value_of = |name: &str| -> Option<String> {
        if name.eq_ignore_ascii_case("bucket") {
            return Some(bucket.to_string());
        }
        field(fields, name).map(str::to_string)
    };
    let mut covered: Vec<String> = Vec::new();
    let mut out = PostPolicy::default();
    let failed = |c: &serde_json::Value| S3Error::msg(Code::AccessDenied, format!("Invalid according to Policy: Policy Condition failed: {c}"));
    for c in conditions {
        match c {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    let want = v.as_str().ok_or_else(|| invalid(format!("Invalid Policy: Invalid Simple-Condition: value must be a string: {c}")))?;
                    if value_of(k).as_deref() != Some(want) {
                        return Err(failed(c));
                    }
                    covered.push(k.to_ascii_lowercase());
                }
            }
            serde_json::Value::Array(a) => {
                let op = a.first().and_then(|v| v.as_str()).map(str::to_ascii_lowercase).ok_or_else(|| invalid(format!("Invalid Policy: Invalid condition {c}")))?;
                if op == "content-length-range" {
                    let n = |i: usize| a.get(i).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
                    let (Some(lo), Some(hi)) = (n(1), n(2)) else { return Err(invalid(format!("Invalid Policy: Invalid content-length-range {c}"))) };
                    out.content_length_range = Some((lo, hi));
                    continue;
                }
                let (Some(name), Some(arg)) = (a.get(1).and_then(|v| v.as_str()), a.get(2).and_then(|v| v.as_str())) else {
                    return Err(invalid(format!("Invalid Policy: Invalid condition {c}")));
                };
                let name = name.strip_prefix('$').ok_or_else(|| invalid(format!("Invalid Policy: field names must start with $: {c}")))?;
                let got = value_of(name);
                let ok = match op.as_str() {
                    "eq" => got.as_deref() == Some(arg),
                    "starts-with" => match &got {
                        Some(g) => g.starts_with(arg),
                        None => arg.is_empty(),
                    },
                    _ => return Err(invalid(format!("Invalid Policy: unknown condition operator {op}"))),
                };
                if !ok {
                    return Err(failed(c));
                }
                covered.push(name.to_ascii_lowercase());
            }
            _ => return Err(invalid(format!("Invalid Policy: Invalid condition {c}"))),
        }
    }
    for (name, _) in fields {
        if !exempt(name) && !covered.contains(&name.to_ascii_lowercase()) {
            return Err(S3Error::msg(Code::AccessDenied, format!("Invalid according to Policy: Extra input fields: {name}")));
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const T: i64 = 1_369_353_600; // 20130524T000000Z

    fn check(method: &str, path: &str, query: &str, headers: &[(&str, &str)], expected_sig: &str) {
        let req = Request { method, path, query, headers };
        let s = parse(&req).unwrap().unwrap();
        assert_eq!(s.signature, expected_sig);
        let key = signing_key(SK, &s.credential.date, &s.credential.region, "s3");
        verify(&req, &s, &key).unwrap();
        authenticate(&req, T + 60, |ak| (ak == AK).then(|| SK.to_string())).unwrap().unwrap();
        // Tampering breaks it.
        let mut tampered: Vec<(&str, &str)> = headers.to_vec();
        tampered.push(("x-amz-date", "20130524T000001Z"));
        let req2 = Request { headers: &tampered, ..req };
        assert!(verify(&req2, &s, &key).is_err());
        assert_eq!(authenticate(&req, T, |_| None).unwrap_err().code, Code::InvalidAccessKeyId);
        assert_eq!(authenticate(&req, T + 3600, |_| Some(SK.into())).unwrap_err().code, Code::RequestTimeTooSkewed);
    }

    #[test]
    fn derive_key() {
        // From the AWS SigV4 documentation.
        let k = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20120215", "us-east-1", "iam");
        assert_eq!(hex::encode(k), "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d");
    }

    // The S3 SigV4 examples ("Examples: Signature Calculations in AWS Signature Version 4").
    #[test]
    fn aws_example_get_object() {
        let auth = "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41";
        let headers = [("Host", "examplebucket.s3.amazonaws.com"), ("Range", "bytes=0-9"), ("x-amz-content-sha256", EMPTY_SHA256), ("x-amz-date", "20130524T000000Z"), ("Authorization", auth)];
        let req = Request { method: "GET", path: "/test.txt", query: "", headers: &headers };
        let s = parse(&req).unwrap().unwrap();
        assert_eq!(
            canonical_request(&req, &s),
            "GET\n/test.txt\n\nhost:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\nx-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\nx-amz-date:20130524T000000Z\n\nhost;range;x-amz-content-sha256;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        check("GET", "/test.txt", "", &headers, "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
        let signed: Vec<(&str, &str)> = headers[..4].to_vec();
        assert_eq!(authorization("GET", "/test.txt", "", &signed, AK, SK, "us-east-1"), auth.replace(",SignedHeaders", ", SignedHeaders").replace(",Signature", ", Signature"));
    }

    #[test]
    fn aws_example_put_object() {
        let auth = "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class,Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd";
        let headers = [
            ("Host", "examplebucket.s3.amazonaws.com"),
            ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("Authorization", auth),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ("x-amz-content-sha256", "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"),
        ];
        check("PUT", "/test%24file.text", "", &headers, "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd");
        assert_eq!(crate::checksum::sha256_hex(b"Welcome to Amazon S3."), "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
    }

    #[test]
    fn aws_example_get_lifecycle() {
        let auth = "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=host;x-amz-content-sha256;x-amz-date,Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543";
        let headers = [("Host", "examplebucket.s3.amazonaws.com"), ("x-amz-date", "20130524T000000Z"), ("x-amz-content-sha256", EMPTY_SHA256), ("Authorization", auth)];
        check("GET", "/", "lifecycle", &headers, "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543");
    }

    #[test]
    fn aws_example_list_objects() {
        let auth = "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=host;x-amz-content-sha256;x-amz-date,Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7";
        let headers = [("Host", "examplebucket.s3.amazonaws.com"), ("x-amz-date", "20130524T000000Z"), ("x-amz-content-sha256", EMPTY_SHA256), ("Authorization", auth)];
        check("GET", "/", "max-keys=2&prefix=J", &headers, "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
        // Parameter order doesn't matter.
        check("GET", "/", "prefix=J&max-keys=2", &headers, "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
    }

    #[test]
    fn aws_example_presigned() {
        let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404";
        let headers = [("Host", "examplebucket.s3.amazonaws.com")];
        let req = Request { method: "GET", path: "/test.txt", query, headers: &headers };
        let s = parse(&req).unwrap().unwrap();
        assert_eq!(s.presigned_expires, Some(86_400));
        assert_eq!(s.payload, Payload::Unsigned);
        authenticate(&req, T + 100, |_| Some(SK.into())).unwrap().unwrap();
        assert_eq!(authenticate(&req, T + 86_401, |_| Some(SK.into())).unwrap_err().code, Code::AccessDenied);
        assert_eq!(authenticate(&req, T - 3600, |_| Some(SK.into())).unwrap_err().code, Code::AccessDenied);
        let url = presign_url(&Presign { method: "GET", endpoint: "https://examplebucket.s3.amazonaws.com", path: "/test.txt", query: &[], access_key: AK, secret: SK, region: "us-east-1", now: T, expires_s: 86_400 });
        assert_eq!(url, format!("https://examplebucket.s3.amazonaws.com/test.txt?{query}"));
    }

    #[test]
    fn presign_round_trip() {
        let url = presign_url(&Presign {
            method: "PUT",
            endpoint: "http://127.0.0.1:59000",
            path: "/bkt/dir/a b+c.txt",
            query: &[("response-content-disposition", "attachment; filename=\"x y\"")],
            access_key: AK,
            secret: SK,
            region: "auto",
            now: T,
            expires_s: 60,
        });
        let rest = url.strip_prefix("http://127.0.0.1:59000").unwrap();
        let (path, query) = rest.split_once('?').unwrap();
        assert_eq!(path, "/bkt/dir/a%20b%2Bc.txt");
        let headers = [("host", "127.0.0.1:59000")];
        let req = Request { method: "PUT", path, query, headers: &headers };
        let v = authenticate(&req, T + 30, |_| Some(SK.into())).unwrap().unwrap();
        assert_eq!(v.access_key, AK);
        assert!(v.stream.is_none());
        let req = Request { method: "GET", ..req };
        assert_eq!(authenticate(&req, T + 30, |_| Some(SK.into())).unwrap_err().code, Code::SignatureDoesNotMatch);
    }

    #[test]
    fn header_parsing_errors() {
        let base = [("host", "h"), ("x-amz-date", "20130524T000000Z"), ("x-amz-content-sha256", UNSIGNED_PAYLOAD)];
        let with = |auth: &'static str| {
            let mut h = base.to_vec();
            h.push(("authorization", auth));
            h
        };
        let p = |h: &[(&str, &str)]| parse(&Request { method: "GET", path: "/", query: "", headers: h }).map_err(|e| e.code);
        assert_eq!(p(&with("AWS AKID:sig")), Err(Code::InvalidRequest));
        assert_eq!(p(&with("Bearer x")), Err(Code::AuthorizationHeaderMalformed));
        assert_eq!(p(&with("AWS4-HMAC-SHA256 Credential=AK/20130524/us-east-1/s3/aws4_request, SignedHeaders=x-amz-date, Signature=ab")), Err(Code::AccessDenied));
        assert_eq!(p(&with("AWS4-HMAC-SHA256 Credential=AK/20130525/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=ab")), Err(Code::AuthorizationHeaderMalformed));
        assert_eq!(p(&with("AWS4-HMAC-SHA256 Credential=AK/20130524/us-east-1/ec2/aws4_request, SignedHeaders=host, Signature=ab")), Err(Code::AuthorizationHeaderMalformed));
        assert_eq!(p(&with("AWS4-HMAC-SHA256 Credential=AK/20130524/us-east-1/s3/aws4_request, SignedHeaders=host")), Err(Code::AuthorizationHeaderMalformed));
        let ok = p(&with("AWS4-HMAC-SHA256 Credential=AK/20130524/auto/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=AB")).unwrap().unwrap();
        assert_eq!(ok.credential.region, "auto");
        assert_eq!(ok.signature, "ab");
        let no_sha = [("host", "h"), ("x-amz-date", "20130524T000000Z"), ("authorization", "AWS4-HMAC-SHA256 Credential=AK/20130524/r/s3/aws4_request, SignedHeaders=host, Signature=ab")];
        assert_eq!(p(&no_sha), Err(Code::InvalidRequest));
        assert_eq!(p(&base), Ok(None));
        assert_eq!(Payload::parse("nope").unwrap_err().code, Code::InvalidArgument);
        assert_eq!(Payload::parse(STREAMING_PAYLOAD_TRAILER).unwrap(), Payload::Streaming { trailer: true });
        let q = |query: &str| parse(&Request { method: "GET", path: "/", query, headers: &[("host", "h")] }).map_err(|e| e.code);
        assert_eq!(q("X-Amz-Algorithm=AWS4-HMAC-SHA256"), Err(Code::AuthorizationQueryParametersError));
        assert_eq!(
            q("X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AK%2F20130524%2Fr%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=604801&X-Amz-SignedHeaders=host&X-Amz-Signature=ab"),
            Err(Code::AuthorizationQueryParametersError)
        );
        assert_eq!(q("AWSAccessKeyId=a&Signature=b&Expires=1"), Err(Code::InvalidRequest));
    }

    #[test]
    fn canonical_forms() {
        assert_eq!(canonical_uri("/a%20b/c+d/%7Ex"), "/a%20b/c%2Bd/~x");
        assert_eq!(canonical_uri("//double//slash/./x"), "//double//slash/./x");
        assert_eq!(canonical_uri(""), "/");
        assert_eq!(canonical_query("b=2&a=1&a=0&c&d=x%2Fy", false), "a=0&a=1&b=2&c=&d=x%2Fy");
        assert_eq!(canonical_query("X-Amz-Signature=abc&b=1", true), "b=1");
        assert_eq!(canonical_header_value("  a   b  c "), "a b c");
    }

    // AWS's example: PUT with a 66560-byte payload in signed 64 KiB chunks.
    pub(crate) fn chunked_example() -> (Vec<(&'static str, &'static str)>, Vec<u8>) {
        let headers = vec![
            ("Host", "s3.amazonaws.com"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ("Authorization", "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=content-encoding;content-length;host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length;x-amz-storage-class,Signature=4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9"),
            ("x-amz-content-sha256", STREAMING_PAYLOAD),
            ("Content-Encoding", "aws-chunked"),
            ("x-amz-decoded-content-length", "66560"),
            ("Content-Length", "66824"),
        ];
        let mut body = Vec::new();
        body.extend_from_slice(b"10000;chunk-signature=ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648\r\n");
        body.extend_from_slice(&[b'a'; 65536]);
        body.extend_from_slice(b"\r\n400;chunk-signature=0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497\r\n");
        body.extend_from_slice(&[b'a'; 1024]);
        body.extend_from_slice(b"\r\n0;chunk-signature=b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9\r\n\r\n");
        (headers, body)
    }

    #[test]
    fn aws_example_chunked_seed() {
        let (headers, body) = chunked_example();
        assert_eq!(body.len(), 66824);
        let req = Request { method: "PUT", path: "/examplebucket/chunkObject.txt", query: "", headers: &headers };
        let v = authenticate(&req, T, |_| Some(SK.into())).unwrap().unwrap();
        assert_eq!(v.payload, Payload::Streaming { trailer: false });
        let st = v.stream.unwrap();
        assert_eq!(st.seed_signature, "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9");
        assert_eq!(st.scope, "20130524/us-east-1/s3/aws4_request");
        let first = chunk_signature(&st.signing_key, &st.amz_date, &st.scope, &st.seed_signature, &crate::checksum::sha256_hex(&[b'a'; 65536]));
        assert_eq!(first, "ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648");
    }

    fn post_fields(policy: &str, sig: &str) -> Vec<(String, String)> {
        [
            ("key", "user/user1/photo.jpg"),
            ("acl", "public-read"),
            ("success_action_redirect", "http://sigv4examplebucket.s3.amazonaws.com/successful_upload.html"),
            ("Content-Type", "image/jpeg"),
            ("x-amz-meta-uuid", "14365123651274"),
            ("x-amz-server-side-encryption", "AES256"),
            ("X-Amz-Credential", "AKIAIOSFODNN7EXAMPLE/20151229/us-east-1/s3/aws4_request"),
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
            ("X-Amz-Date", "20151229T000000Z"),
            ("x-amz-meta-tag", ""),
            ("Policy", policy),
            ("X-Amz-Signature", sig),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    // AWS's browser-based upload example (POST with SigV4).
    #[test]
    fn aws_example_post_policy() {
        let policy = "eyAiZXhwaXJhdGlvbiI6ICIyMDE1LTEyLTMwVDEyOjAwOjAwLjAwMFoiLA0KICAiY29uZGl0aW9ucyI6IFsNCiAgICB7ImJ1Y2tldCI6ICJzaWd2NGV4YW1wbGVidWNrZXQifSwNCiAgICBbInN0YXJ0cy13aXRoIiwgIiRrZXkiLCAidXNlci91c2VyMS8iXSwNCiAgICB7ImFjbCI6ICJwdWJsaWMtcmVhZCJ9LA0KICAgIHsic3VjY2Vzc19hY3Rpb25fcmVkaXJlY3QiOiAiaHR0cDovL3NpZ3Y0ZXhhbXBsZWJ1Y2tldC5zMy5hbWF6b25hd3MuY29tL3N1Y2Nlc3NmdWxfdXBsb2FkLmh0bWwifSwNCiAgICBbInN0YXJ0cy13aXRoIiwgIiRDb250ZW50LVR5cGUiLCAiaW1hZ2UvIl0sDQogICAgeyJ4LWFtei1tZXRhLXV1aWQiOiAiMTQzNjUxMjM2NTEyNzQifSwNCiAgICB7IngtYW16LXNlcnZlci1zaWRlLWVuY3J5cHRpb24iOiAiQUVTMjU2In0sDQogICAgWyJzdGFydHMtd2l0aCIsICIkeC1hbXotbWV0YS10YWciLCAiIl0sDQoNCiAgICB7IngtYW16LWNyZWRlbnRpYWwiOiAiQUtJQUlPU0ZPRE5ON0VYQU1QTEUvMjAxNTEyMjkvdXMtZWFzdC0xL3MzL2F3czRfcmVxdWVzdCJ9LA0KICAgIHsieC1hbXotYWxnb3JpdGhtIjogIkFXUzQtSE1BQy1TSEEyNTYifSwNCiAgICB7IngtYW16LWRhdGUiOiAiMjAxNTEyMjlUMDAwMDAwWiIgfQ0KICBdDQp9";
        let sig = "8afdbf4008c03f22c2cd3cdb72e4afbb1f6a588f3255ac628749a66d7f09699e";
        let fields = post_fields(policy, sig);
        let auth = parse_post_auth(&fields).unwrap().unwrap();
        assert_eq!(auth.credential.date, "20151229");
        let key = signing_key(SK, &auth.credential.date, &auth.credential.region, "s3");
        verify_post_signature(&auth, &key).unwrap();
        let now = time::parse_iso8601("2015-12-29T10:00:00Z").unwrap();
        check_post_policy(policy, &fields, "sigv4examplebucket", now).unwrap();
        assert_eq!(check_post_policy(policy, &fields, "other", now).unwrap_err().code, Code::AccessDenied);
        assert_eq!(check_post_policy(policy, &fields, "sigv4examplebucket", now + 2 * 86_400_000).unwrap_err().code, Code::AccessDenied);
        let mut extra = fields.clone();
        extra.push(("x-amz-meta-other".into(), "1".into()));
        assert_eq!(check_post_policy(policy, &extra, "sigv4examplebucket", now).unwrap_err().code, Code::AccessDenied);
        let mut ignored = fields.clone();
        ignored.push(("x-ignore-me".into(), "1".into()));
        check_post_policy(policy, &ignored, "sigv4examplebucket", now).unwrap();
        let mut wrong_type = fields.clone();
        wrong_type[3].1 = "text/plain".into();
        assert_eq!(check_post_policy(policy, &wrong_type, "sigv4examplebucket", now).unwrap_err().code, Code::AccessDenied);
        let bad = post_fields(policy, &"0".repeat(64));
        assert_eq!(verify_post_signature(&parse_post_auth(&bad).unwrap().unwrap(), &key).unwrap_err().code, Code::SignatureDoesNotMatch);
        assert_eq!(parse_post_auth(&[("key".into(), "x".into())]).unwrap(), None);
    }

    #[test]
    fn post_policy_length_range() {
        let doc = r#"{"expiration":"2030-01-01T00:00:00Z","conditions":[{"bucket":"b"},["starts-with","$key",""],["content-length-range",1,"100"],["eq","$x-amz-meta-a","1"]]}"#;
        let p = crate::checksum::b64(doc.as_bytes());
        let fields = vec![("key".to_string(), "k".to_string()), ("x-amz-meta-a".to_string(), "1".to_string())];
        assert_eq!(check_post_policy(&p, &fields, "b", 0).unwrap().content_length_range, Some((1, 100)));
        assert_eq!(check_post_policy("!!", &fields, "b", 0).unwrap_err().code, Code::InvalidPolicyDocument);
        let p2 = crate::checksum::b64(br#"{"conditions":[]}"#);
        assert_eq!(check_post_policy(&p2, &fields, "b", 0).unwrap_err().code, Code::InvalidPolicyDocument);
    }
}
