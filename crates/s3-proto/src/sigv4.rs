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
/// SigV4a's signed `aws-chunked` payloads (ECDSA chunk signatures).
pub const STREAMING_ECDSA_PAYLOAD: &str = "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD";
pub const STREAMING_ECDSA_PAYLOAD_TRAILER: &str = "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD-TRAILER";
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
    /// The same signed with SigV4a: ECDSA chunk (and trailer) signatures.
    StreamingEcdsa { trailer: bool },
    /// `aws-chunked`, unsigned chunks, trailers.
    StreamingUnsignedTrailer,
}

impl Payload {
    pub fn parse(v: &str) -> Result<Self, S3Error> {
        Ok(match v.trim() {
            UNSIGNED_PAYLOAD => Payload::Unsigned,
            STREAMING_PAYLOAD => Payload::Streaming { trailer: false },
            STREAMING_PAYLOAD_TRAILER => Payload::Streaming { trailer: true },
            STREAMING_ECDSA_PAYLOAD => Payload::StreamingEcdsa { trailer: false },
            STREAMING_ECDSA_PAYLOAD_TRAILER => Payload::StreamingEcdsa { trailer: true },
            STREAMING_UNSIGNED_TRAILER => Payload::StreamingUnsignedTrailer,
            s if s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()) => Payload::Sha256(s.to_ascii_lowercase()),
            _ => return Err(S3Error::msg(Code::InvalidArgument, "x-amz-content-sha256 must be UNSIGNED-PAYLOAD, STREAMING-AWS4-HMAC-SHA256-PAYLOAD, STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER, STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD, STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD-TRAILER, STREAMING-UNSIGNED-PAYLOAD-TRAILER or a valid sha256 value.").with("ArgumentName", "x-amz-content-sha256")),
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            Payload::Unsigned => UNSIGNED_PAYLOAD,
            Payload::Sha256(h) => h,
            Payload::Streaming { trailer: false } => STREAMING_PAYLOAD,
            Payload::Streaming { trailer: true } => STREAMING_PAYLOAD_TRAILER,
            Payload::StreamingEcdsa { trailer: false } => STREAMING_ECDSA_PAYLOAD,
            Payload::StreamingEcdsa { trailer: true } => STREAMING_ECDSA_PAYLOAD_TRAILER,
            Payload::StreamingUnsignedTrailer => STREAMING_UNSIGNED_TRAILER,
        }
    }

    /// The body is `aws-chunked`.
    pub fn is_chunked(&self) -> bool {
        matches!(self, Payload::Streaming { .. } | Payload::StreamingEcdsa { .. } | Payload::StreamingUnsignedTrailer)
    }

    /// Signed chunks (HMAC or ECDSA): `Some(with trailers)`.
    pub fn signed_chunks(&self) -> Option<bool> {
        match self {
            Payload::Streaming { trailer } | Payload::StreamingEcdsa { trailer } => Some(*trailer),
            _ => None,
        }
    }
}

/// Requests to S3 may also be signed for `s3-object-lambda` (S3 Object Lambda access points
/// and `WriteGetObjectResponse`, which share S3's endpoint).
pub(crate) fn service_alias(expected: &str, given: &str) -> bool {
    expected == "s3" && given == "s3-object-lambda"
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
        Self::parse_for(s, code, "s3")
    }

    /// [`Self::parse`] for another service on this endpoint (`sts`).
    pub fn parse_for(s: &str, code: Code, service: &str) -> Result<Self, S3Error> {
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
        if parts[3] != service && !service_alias(service, parts[3]) {
            return Err(bad(&format!("incorrect service \"{}\". This endpoint belongs to \"{service}\".", parts[3])));
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
    pub key: ChunkKey,
    pub seed_signature: String,
    pub amz_date: String,
    pub scope: String,
}

/// What checks chunk signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkKey {
    /// SigV4: the request's HMAC signing key.
    Hmac([u8; 32]),
    /// SigV4a: the public key of the access key's derived ECDSA key.
    Ecdsa(p256::ecdsa::VerifyingKey),
}

impl StreamSigning {
    /// SigV4 (HMAC) chunk signing.
    pub fn hmac(signing_key: [u8; 32], seed_signature: String, amz_date: String, scope: String) -> Self {
        Self { key: ChunkKey::Hmac(signing_key), seed_signature, amz_date, scope }
    }

    /// The HMAC signing key (`None` for SigV4a).
    pub fn signing_key(&self) -> Option<&[u8; 32]> {
        match &self.key {
            ChunkKey::Hmac(k) => Some(k),
            ChunkKey::Ecdsa(_) => None,
        }
    }

    /// Check one chunk's signature (`previous`: the signature it chains from).
    pub fn verify_chunk(&self, previous: &str, chunk_sha256_hex: &str, signature: &str) -> bool {
        match &self.key {
            ChunkKey::Hmac(k) => constant_time_eq(&chunk_signature(k, &self.amz_date, &self.scope, previous, chunk_sha256_hex), signature),
            ChunkKey::Ecdsa(vk) => crate::sigv4a::verify_chunk(vk, &self.amz_date, &self.scope, previous, chunk_sha256_hex, signature),
        }
    }

    /// Check the trailers' signature.
    pub fn verify_trailer(&self, previous: &str, trailers: &[(String, String)], signature: &str) -> bool {
        match &self.key {
            ChunkKey::Hmac(k) => constant_time_eq(&trailer_signature(k, &self.amz_date, &self.scope, previous, trailers), signature),
            ChunkKey::Ecdsa(vk) => crate::sigv4a::verify_trailer(vk, &self.amz_date, &self.scope, previous, trailers, signature),
        }
    }
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

pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn malformed(why: impl Into<String>) -> S3Error {
    S3Error::msg(Code::AuthorizationHeaderMalformed, why)
}

/// Parse how a SigV4 request is signed: `None` for requests that carry no SigV4 signature
/// (anonymous, or another scheme: see [`crate::auth::scheme`]). Doesn't check the clock or the
/// signature.
pub fn parse(req: &Request) -> Result<Option<Signed>, S3Error> {
    parse_with(req, "s3", None)
}

/// [`parse`] for another service on the same endpoint (STS): the credential scope must name
/// `service`, and the payload is the body whose SHA-256 (hex) is `body_sha256`. Such services
/// sign the payload without sending `x-amz-content-sha256`.
pub fn parse_service(req: &Request, service: &str, body_sha256: &str) -> Result<Option<Signed>, S3Error> {
    parse_with(req, service, Some(body_sha256))
}

fn parse_with(req: &Request, service: &str, body_sha256: Option<&str>) -> Result<Option<Signed>, S3Error> {
    if let Some(auth) = req.header("authorization") {
        return parse_header(req, auth, service, body_sha256).map(Some);
    }
    let query = crate::http::parse_query(req.query);
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if q("X-Amz-Algorithm").is_some() || q("X-Amz-Signature").is_some() || q("X-Amz-Credential").is_some() {
        return parse_presigned(&query, service, body_sha256).map(Some);
    }
    if q("Signature").is_some() && q("AWSAccessKeyId").is_some() {
        return Err(S3Error::msg(Code::InvalidRequest, "This is a Signature Version 2 request, not AWS4-HMAC-SHA256."));
    }
    Ok(None)
}

/// The `Credential=`, `SignedHeaders=` and `Signature=` components of a SigV4(a)
/// `Authorization` value (after the algorithm).
pub(crate) fn components(rest: &str) -> Result<(&str, Vec<String>, String), S3Error> {
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
    let cred = cred.ok_or_else(|| malformed("The authorization header is malformed; missing Credential."))?;
    let signed_headers: Vec<String> = signed_headers.ok_or_else(|| malformed("The authorization header is malformed; missing SignedHeaders."))?.split(';').map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()).collect();
    let signature = signature.ok_or_else(|| malformed("The authorization header is malformed; missing Signature."))?.to_ascii_lowercase();
    if !signed_headers.iter().any(|h| h == "host") {
        return Err(S3Error::msg(Code::AccessDenied, "There were headers present in the request which were not signed").with("HeadersNotSigned", "host"));
    }
    Ok((cred, signed_headers, signature))
}

/// The request time of a header-signed request: `x-amz-date`, else `Date`.
pub(crate) fn request_time(req: &Request) -> Result<(String, i64), S3Error> {
    let bad = || S3Error::msg(Code::AccessDenied, "AWS authentication requires a valid Date or x-amz-date header");
    Ok(match req.header("x-amz-date") {
        Some(d) => (d.trim().to_string(), time::parse_amz_date(d.trim()).ok_or_else(bad)?),
        None => {
            // botocore signs with a `Date` like `Thu, 22 Oct 2026 10:33:26 -0000` when the
            // request has one: numeric zones are allowed, as in SigV2.
            let t = crate::sigv2::parse_date(req.header("date").ok_or_else(bad)?).ok_or_else(bad)?;
            (time::amz_date(t), t)
        }
    })
}

fn parse_header(req: &Request, auth: &str, service: &str, body_sha256: Option<&str>) -> Result<Signed, S3Error> {
    let auth = auth.trim();
    if auth.starts_with("AWS ") {
        return Err(S3Error::msg(Code::InvalidRequest, "This is a Signature Version 2 request, not AWS4-HMAC-SHA256."));
    }
    let rest = auth.strip_prefix(ALGORITHM).ok_or_else(|| malformed("Unsupported authorization type"))?;
    let (cred, signed_headers, signature) = components(rest)?;
    let credential = Credential::parse_for(cred, Code::AuthorizationHeaderMalformed, service)?;
    let (amz_date, timestamp) = request_time(req)?;
    if amz_date[..8] != credential.date {
        return Err(malformed(format!("The authorization header is malformed; Invalid credential date \"{}\". This date is not the same as X-Amz-Date: \"{}\".", credential.date, &amz_date[..8])));
    }
    if let Some(h) = body_sha256 {
        return Ok(Signed { credential, signed_headers, signature, amz_date, timestamp, payload: Payload::Sha256(h.to_ascii_lowercase()), presigned_expires: None });
    }
    let payload = Payload::parse(req.header("x-amz-content-sha256").ok_or_else(|| S3Error::msg(Code::InvalidRequest, "Missing required header for this request: x-amz-content-sha256"))?)?;
    if matches!(payload, Payload::StreamingEcdsa { .. }) {
        return Err(S3Error::msg(Code::InvalidRequest, "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD needs an AWS4-ECDSA-P256-SHA256 signature.").with("Header", "x-amz-content-sha256"));
    }
    Ok(Signed { credential, signed_headers, signature, amz_date, timestamp, payload, presigned_expires: None })
}

fn parse_presigned(query: &[(String, String)], service: &str, body_sha256: Option<&str>) -> Result<Signed, S3Error> {
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let missing = || S3Error::msg(Code::AuthorizationQueryParametersError, "Query-string authentication version 4 requires the X-Amz-Algorithm, X-Amz-Credential, X-Amz-Signature, X-Amz-Date, X-Amz-SignedHeaders, and X-Amz-Expires parameters.");
    if q("X-Amz-Algorithm").ok_or_else(missing)? != ALGORITHM {
        return Err(S3Error::msg(Code::AuthorizationQueryParametersError, "X-Amz-Algorithm only supports \"AWS4-HMAC-SHA256\" and \"AWS4-ECDSA-P256-SHA256\""));
    }
    let credential = Credential::parse_for(q("X-Amz-Credential").ok_or_else(missing)?, Code::AuthorizationQueryParametersError, service)?;
    let mut s = presigned_rest(query, &credential.date)?;
    s.credential = credential;
    if let Some(h) = body_sha256 {
        s.payload = Payload::Sha256(h.to_ascii_lowercase());
    }
    Ok(s)
}

/// The parts of a presigned query string after the algorithm and credential (`credential` left
/// for the caller to fill in).
pub(crate) fn presigned_rest(query: &[(String, String)], credential_date: &str) -> Result<Signed, S3Error> {
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let missing = || S3Error::msg(Code::AuthorizationQueryParametersError, "Query-string authentication version 4 requires the X-Amz-Algorithm, X-Amz-Credential, X-Amz-Signature, X-Amz-Date, X-Amz-SignedHeaders, and X-Amz-Expires parameters.");
    let amz_date = q("X-Amz-Date").ok_or_else(missing)?.to_string();
    let timestamp = time::parse_amz_date(&amz_date).ok_or_else(|| S3Error::msg(Code::AuthorizationQueryParametersError, "X-Amz-Date must be in the ISO8601 Long Format \"yyyyMMdd'T'HHmmss'Z'\""))?;
    if amz_date[..8] != *credential_date {
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
    let credential = Credential { access_key: String::new(), date: credential_date.to_string(), region: String::new(), service: String::new() };
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
    let stream = matches!(s.payload, Payload::Streaming { .. }).then(|| StreamSigning::hmac(*signing_key, s.signature.clone(), s.amz_date.clone(), s.credential.scope()));
    Ok(Verified { access_key: s.credential.access_key.clone(), payload: s.payload.clone(), stream })
}

/// Everything in one call for SigV4 requests: `Ok(None)` for requests without a SigV4
/// signature; `lookup` maps an access key to its secret (`InvalidAccessKeyId` if it returns
/// `None`). [`crate::auth::authenticate`] handles every scheme.
pub fn authenticate(req: &Request, now: i64, lookup: impl FnOnce(&str) -> Option<String>) -> Result<Option<Verified>, S3Error> {
    let Some(s) = parse(req)? else { return Ok(None) };
    let secret = lookup(&s.credential.access_key).ok_or_else(|| S3Error::new(Code::InvalidAccessKeyId).with("AWSAccessKeyId", s.credential.access_key.clone()))?;
    check_time(&s, now)?;
    let key = signing_key(&secret, &s.credential.date, &s.credential.region, &s.credential.service);
    verify(req, &s, &key).map(Some)
}

/// [`authenticate`] for another service on this endpoint (STS; see [`parse_service`]).
pub fn authenticate_service(req: &Request, now: i64, service: &str, body_sha256: &str, lookup: impl FnOnce(&str) -> Option<String>) -> Result<Option<Verified>, S3Error> {
    let Some(s) = parse_service(req, service, body_sha256)? else { return Ok(None) };
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

/// Fields that never need a policy condition (`signature` and `AWSAccessKeyId` are a SigV2
/// form's).
fn exempt(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "x-amz-signature" || n == "file" || n == "policy" || n == "signature" || n == "awsaccesskeyid" || n.starts_with("x-ignore-")
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
                if m.len() != 1 {
                    return Err(invalid(format!("Invalid Policy: Invalid Simple-Condition: Simple-Conditions must have exactly one property specified: {c}")));
                }
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

    /// Another service on this endpoint (STS): its scope, and the body as the payload without
    /// `x-amz-content-sha256`.
    #[test]
    fn other_services() {
        let body = b"Action=GetCallerIdentity&Version=2011-06-15";
        let hash = hex::encode(Sha256::digest(body));
        let headers = [("host", "h"), ("x-amz-date", "20130524T000000Z"), ("content-type", "application/x-www-form-urlencoded")];
        let mut s = Signed {
            credential: Credential { access_key: AK.into(), date: "20130524".into(), region: "us-east-1".into(), service: "sts".into() },
            signed_headers: vec!["content-type".into(), "host".into(), "x-amz-date".into()],
            signature: String::new(),
            amz_date: "20130524T000000Z".into(),
            timestamp: T,
            payload: Payload::Sha256(hash.clone()),
            presigned_expires: None,
        };
        let req = Request { method: "POST", path: "/", query: "", headers: &headers };
        let key = signing_key(SK, "20130524", "us-east-1", "sts");
        s.signature = sign(&key, &string_to_sign(&s.amz_date, &s.credential.scope(), &canonical_request(&req, &s)));
        let auth = format!("{ALGORITHM} Credential={AK}/20130524/us-east-1/sts/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature={}", s.signature);
        let mut h = headers.to_vec();
        h.push(("authorization", &auth));
        let req = Request { headers: &h, ..req };
        authenticate_service(&req, T, "sts", &hash, |_| Some(SK.into())).unwrap().unwrap();
        // The body is what's signed, and the scope must be the service's.
        let other = hex::encode(Sha256::digest(b"Action=AssumeRole"));
        assert_eq!(authenticate_service(&req, T, "sts", &other, |_| Some(SK.into())).unwrap_err().code, Code::SignatureDoesNotMatch);
        assert_eq!(authenticate_service(&req, T, "iam", &hash, |_| Some(SK.into())).unwrap_err().code, Code::AuthorizationHeaderMalformed);
        assert_eq!(parse(&req).unwrap_err().code, Code::AuthorizationHeaderMalformed, "S3 refuses an sts scope");
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
        let first = chunk_signature(st.signing_key().unwrap(), &st.amz_date, &st.scope, &st.seed_signature, &crate::checksum::sha256_hex(&[b'a'; 65536]));
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

    #[test]
    fn credential_errors() {
        let c = |s: &str| Credential::parse(s, Code::AuthorizationHeaderMalformed).map_err(|e| e.code);
        assert_eq!(c(" AK/20130524/us-east-1/s3/aws4_request ").unwrap().scope(), "20130524/us-east-1/s3/aws4_request");
        for bad in ["AK/20130524/us-east-1/s3", "AK/20130524/us-east-1/s3/aws4", "/20130524/us-east-1/s3/aws4_request", "AK/2013052/us-east-1/s3/aws4_request", "AK/2013O524/us-east-1/s3/aws4_request", "AK/20130524/us-east-1/s3/aws4_request/x", ""] {
            assert_eq!(c(bad), Err(Code::AuthorizationHeaderMalformed), "{bad:?}");
        }
        assert_eq!(Credential::parse("x", Code::AuthorizationQueryParametersError).unwrap_err().code, Code::AuthorizationQueryParametersError);
    }

    #[test]
    fn header_components_and_dates() {
        let p = |auth: &str, date: (&str, &str), sha: &str| parse(&Request { method: "GET", path: "/", query: "", headers: &[("host", "h"), date, ("x-amz-content-sha256", sha), ("authorization", auth)] }).map_err(|e| e.code);
        let ok = "AWS4-HMAC-SHA256 Credential=AK/20130524/r/s3/aws4_request, SignedHeaders=host;x-amz-date, Signature=ab";
        let d = ("x-amz-date", "20130524T000000Z");
        assert!(p(ok, d, UNSIGNED_PAYLOAD).unwrap().is_some());
        assert_eq!(p("AWS4-HMAC-SHA256 SignedHeaders=host, Signature=ab", d, UNSIGNED_PAYLOAD), Err(Code::AuthorizationHeaderMalformed));
        assert_eq!(p("AWS4-HMAC-SHA256 Credential=AK/20130524/r/s3/aws4_request, Signature=ab", d, UNSIGNED_PAYLOAD), Err(Code::AuthorizationHeaderMalformed));
        assert_eq!(p("AWS4-HMAC-SHA256 Credential=AK/20130524/r/s3/aws4_request, SignedHeaders, Signature=ab", d, UNSIGNED_PAYLOAD), Err(Code::AuthorizationHeaderMalformed));
        let s = p("AWS4-HMAC-SHA256 Credential=AK/20130524/r/s3/aws4_request,,Foo=bar, SignedHeaders=Host; X-Amz-Date, Signature=ab", d, UNSIGNED_PAYLOAD).unwrap().unwrap();
        assert_eq!(s.signed_headers, vec!["host", "x-amz-date"], "unknown components are ignored, names lowercased");
        // Dates: x-amz-date, else Date; anything unparseable is refused (never a panic).
        for bad in [("x-amz-date", "2013-05-24"), ("x-amz-date", "201é524T000000Z"), ("date", "garbage"), ("x-other", "")] {
            assert_eq!(p(ok, bad, UNSIGNED_PAYLOAD), Err(Code::AccessDenied), "{bad:?}");
        }
        let s = p(ok, ("date", "Fri, 24 May 2013 00:00:00 GMT"), UNSIGNED_PAYLOAD).unwrap().unwrap();
        assert_eq!((s.amz_date.as_str(), s.timestamp), ("20130524T000000Z", T));
        // botocore's `Date` when the request already has one (numeric zone).
        let s = p(ok, ("date", "Fri, 24 May 2013 00:00:00 -0000"), UNSIGNED_PAYLOAD).unwrap().unwrap();
        assert_eq!((s.amz_date.as_str(), s.timestamp), ("20130524T000000Z", T));
        // Payload hashes.
        assert_eq!(p(ok, d, STREAMING_ECDSA_PAYLOAD), Err(Code::InvalidRequest), "ECDSA chunks need a SigV4a signature");
        assert_eq!(p(ok, d, &EMPTY_SHA256.to_uppercase()).unwrap().unwrap().payload, Payload::Sha256(EMPTY_SHA256.into()));
        assert_eq!(p(ok, d, &EMPTY_SHA256[1..]), Err(Code::InvalidArgument));
        for v in [UNSIGNED_PAYLOAD, STREAMING_PAYLOAD, STREAMING_PAYLOAD_TRAILER, STREAMING_ECDSA_PAYLOAD, STREAMING_ECDSA_PAYLOAD_TRAILER, STREAMING_UNSIGNED_TRAILER, EMPTY_SHA256] {
            let pl = Payload::parse(v).unwrap();
            assert_eq!(pl.as_str(), v);
            assert_eq!(pl.is_chunked(), v.starts_with("STREAMING"));
        }
    }

    #[test]
    fn presigned_parameter_errors() {
        type Q = Vec<(&'static str, &'static str)>;
        let base: Q = vec![
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
            ("X-Amz-Credential", "AK%2F20130524%2Fr%2Fs3%2Faws4_request"),
            ("X-Amz-Date", "20130524T000000Z"),
            ("X-Amz-Expires", "60"),
            ("X-Amz-SignedHeaders", "host"),
            ("X-Amz-Signature", "ab"),
        ];
        let pq = |f: &dyn Fn(&mut Q)| {
            let mut v = base.clone();
            f(&mut v);
            let query = v.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
            parse(&Request { method: "GET", path: "/", query: &query, headers: &[("host", "h")] }).map(Option::unwrap).map_err(|e| e.code)
        };
        let set = |k: &'static str, val: &'static str| {
            move |v: &mut Q| {
                v.retain(|(n, _)| *n != k);
                v.push((k, val));
            }
        };
        let without = |k: &'static str| move |v: &mut Q| v.retain(|(n, _)| *n != k);
        assert_eq!(pq(&|_| {}).unwrap().presigned_expires, Some(60));
        let qpe = Err(Code::AuthorizationQueryParametersError);
        assert_eq!(pq(&set("X-Amz-Algorithm", "AWS4-HMAC-SHA1")), qpe);
        assert_eq!(pq(&set("X-Amz-Date", "20130524")), qpe);
        assert_eq!(pq(&set("X-Amz-Date", "201%C3%A9524T000000Z")), qpe);
        assert_eq!(pq(&set("X-Amz-Date", "20130525T000000Z")), qpe, "credential date differs");
        assert_eq!(pq(&set("X-Amz-Expires", "soon")), qpe);
        assert_eq!(pq(&set("X-Amz-Expires", "0")), qpe);
        assert_eq!(pq(&set("X-Amz-Expires", "-5")), qpe);
        assert_eq!(pq(&set("X-Amz-Expires", "604800")).unwrap().presigned_expires, Some(MAX_PRESIGN_S));
        assert_eq!(pq(&set("X-Amz-Credential", "AK%2F20130524%2Fr%2Fsqs%2Faws4_request")), qpe);
        assert_eq!(pq(&set("X-Amz-SignedHeaders", "x-amz-date")), Err(Code::AccessDenied));
        for k in ["X-Amz-Algorithm", "X-Amz-Credential", "X-Amz-Date", "X-Amz-Expires", "X-Amz-SignedHeaders", "X-Amz-Signature"] {
            assert_eq!(pq(&without(k)), qpe, "{k}");
        }
        assert_eq!(pq(&set("X-Amz-Content-Sha256", STREAMING_PAYLOAD)), Err(Code::InvalidRequest));
        assert_eq!(pq(&set("X-Amz-Content-Sha256", STREAMING_UNSIGNED_TRAILER)), Err(Code::InvalidRequest));
        assert_eq!(pq(&set("X-Amz-Content-Sha256", EMPTY_SHA256)).unwrap().payload, Payload::Sha256(EMPTY_SHA256.into()));
        assert_eq!(pq(&set("X-Amz-Content-Sha256", "junk")), Err(Code::InvalidArgument));
    }

    #[test]
    fn clock_checks() {
        let s = |presigned: Option<i64>| Signed {
            credential: Credential { access_key: AK.into(), date: "20130524".into(), region: "r".into(), service: "s3".into() },
            signed_headers: vec!["host".into()],
            signature: String::new(),
            amz_date: time::amz_date(T),
            timestamp: T,
            payload: Payload::Unsigned,
            presigned_expires: presigned,
        };
        let at = |p: Option<i64>, now: i64| check_time(&s(p), now).map_err(|e| e.code);
        assert_eq!(at(None, T + MAX_SKEW_S), Ok(()));
        assert_eq!(at(None, T - MAX_SKEW_S), Ok(()));
        assert_eq!(at(None, T + MAX_SKEW_S + 1), Err(Code::RequestTimeTooSkewed), "too old");
        assert_eq!(at(None, T - MAX_SKEW_S - 1), Err(Code::RequestTimeTooSkewed), "from the future");
        let e = check_time(&s(None), T + 3600).unwrap_err();
        assert!(e.extra.contains(&("MaxAllowedSkewMilliseconds", "900000".into())));
        assert_eq!(at(Some(60), T + 60), Ok(()));
        assert_eq!(at(Some(60), T + 61), Err(Code::AccessDenied));
        assert_eq!(at(Some(60), T - MAX_SKEW_S), Ok(()), "a little ahead of the server is fine");
        assert_eq!(at(Some(60), T - MAX_SKEW_S - 1), Err(Code::AccessDenied), "not valid yet");
    }

    #[test]
    fn verify_covers_signed_parts() {
        let headers = [("host", "h:9000"), ("x-amz-date", "20130524T000000Z"), ("x-amz-content-sha256", UNSIGNED_PAYLOAD), ("x-amz-meta-a", " 1  2 ")];
        let auth = authorization("PUT", "/b/a%20k", "x=1&y", &headers, AK, SK, "eu-west-1");
        let mut with = headers.to_vec();
        with.push(("authorization", auth.as_str()));
        let req = Request { method: "PUT", path: "/b/a%20k", query: "x=1&y", headers: &with };
        let auth_at = |r: &Request| authenticate(r, T, |_| Some(SK.into())).map_err(|e| e.code);
        let v = auth_at(&req).unwrap().unwrap();
        assert_eq!((v.access_key.as_str(), v.payload.clone(), v.stream.is_none()), (AK, Payload::Unsigned, true));
        // Unsigned headers can change; everything signed can't.
        let mut extra = with.clone();
        extra.push(("x-unsigned", "z"));
        assert!(auth_at(&Request { headers: &extra, ..req }).is_ok());
        let missing: Vec<_> = with.iter().filter(|(k, _)| *k != "x-amz-meta-a").copied().collect();
        assert_eq!(auth_at(&Request { headers: &missing, ..req }), Err(Code::SignatureDoesNotMatch));
        let changed: Vec<_> = with.iter().map(|(k, v)| if *k == "x-amz-meta-a" { (*k, "1 2") } else { (*k, *v) }).collect();
        assert!(auth_at(&Request { headers: &changed, ..req }).is_ok(), "whitespace is folded");
        let sha: Vec<_> = with.iter().map(|(k, v)| if *k == "x-amz-content-sha256" { (*k, EMPTY_SHA256) } else { (*k, *v) }).collect();
        assert_eq!(auth_at(&Request { headers: &sha, ..req }), Err(Code::SignatureDoesNotMatch), "the payload hash is signed");
        assert_eq!(auth_at(&Request { query: "x=2&y", ..req }), Err(Code::SignatureDoesNotMatch));
        assert_eq!(auth_at(&Request { path: "/b/a%20j", ..req }), Err(Code::SignatureDoesNotMatch));
        assert_eq!(auth_at(&Request { method: "POST", ..req }), Err(Code::SignatureDoesNotMatch));
        assert_eq!(authenticate(&req, T, |_| Some("wrong".into())).unwrap_err().code, Code::SignatureDoesNotMatch);
        // The key is checked before the clock, so an unknown key says so even when stale.
        assert_eq!(authenticate(&req, T + 3600, |_| None).unwrap_err().code, Code::InvalidAccessKeyId);
        let e = authenticate(&req, T, |_| Some("wrong".into())).unwrap_err();
        assert!(e.extra.iter().any(|(k, _)| *k == "CanonicalRequest") && e.extra.iter().any(|(k, _)| *k == "StringToSign"));
        // Signed chunked payloads carry their signing context; unsigned-trailer ones don't need one.
        for (payload, stream) in [(STREAMING_PAYLOAD, true), (STREAMING_PAYLOAD_TRAILER, true), (STREAMING_UNSIGNED_TRAILER, false)] {
            let h = [("host", "h"), ("x-amz-date", "20130524T000000Z"), ("x-amz-content-sha256", payload)];
            let auth = authorization("PUT", "/b/k", "", &h, AK, SK, "auto");
            let mut with = h.to_vec();
            with.push(("authorization", auth.as_str()));
            let v = auth_at(&Request { method: "PUT", path: "/b/k", query: "", headers: &with }).unwrap().unwrap();
            assert_eq!(v.stream.is_some(), stream, "{payload}");
            assert!(v.payload.is_chunked());
        }
    }

    #[test]
    fn presign_hosts_and_lifetimes() {
        for (endpoint, host) in [("http://example.com:80", "example.com"), ("https://example.com:443/", "example.com"), ("http://example.com:8080", "example.com:8080"), ("https://example.com:80", "example.com:80"), ("example.com", "example.com")] {
            let url = presign_url(&Presign { method: "GET", endpoint, path: "/b/k", query: &[], access_key: AK, secret: SK, region: "auto", now: T, expires_s: 10 * MAX_PRESIGN_S });
            let (_, query) = url.split_once("/b/k?").unwrap();
            assert!(query.contains("X-Amz-Expires=604800"), "clamped to a week: {url}");
            let headers = [("host", host)];
            authenticate(&Request { method: "GET", path: "/b/k", query, headers: &headers }, T + 10, |_| Some(SK.into())).unwrap().unwrap();
        }
        let url = presign_url(&Presign { method: "GET", endpoint: "http://h", path: "/b/k", query: &[], access_key: AK, secret: SK, region: "auto", now: T, expires_s: 0 });
        assert!(url.contains("X-Amz-Expires=1&"), "at least a second: {url}");
    }

    #[test]
    fn post_auth_fields() {
        let full: Vec<(String, String)> = [("policy", "p"), ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"), ("x-amz-credential", "AK/20130524/r/s3/aws4_request"), ("x-amz-date", "20130524T000000Z"), ("x-amz-signature", "AB")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let a = parse_post_auth(&full).unwrap().unwrap();
        assert_eq!((a.signature.as_str(), a.credential.region.as_str(), a.policy.as_str()), ("ab", "r", "p"));
        for i in 1..full.len() {
            let mut f = full.clone();
            f.remove(i);
            assert_eq!(parse_post_auth(&f).unwrap_err().code, Code::InvalidArgument, "without {}", full[i].0);
        }
        let mut f = full.clone();
        f[1].1 = "AWS4-ECDSA-P256-SHA256".into();
        assert_eq!(parse_post_auth(&f).unwrap_err().code, Code::InvalidArgument);
        let mut f = full.clone();
        f[2].1 = "AK/x".into();
        assert_eq!(parse_post_auth(&f).unwrap_err().code, Code::InvalidArgument);
    }

    #[test]
    fn post_policy_conditions() {
        let owned = |f: &[(&str, &str)]| f.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let fields = owned(&[("key", "uploads/a.txt"), ("Content-Type", "text/plain"), ("x-amz-meta-a", "1"), ("file", ""), ("policy", "p"), ("x-amz-signature", "s"), ("X-Ignore-Me", "1")]);
        let doc = |conds: &str| crate::checksum::b64(format!(r#"{{"expiration":"2030-01-01T00:00:00.000Z","conditions":[{conds}]}}"#).as_bytes());
        let check = |conds: &str, fields: &[(String, String)], now: i64| check_post_policy(&doc(conds), fields, "b", now).map(|p| p.content_length_range).map_err(|e| e.code);
        let all = r#"{"bucket":"b"},["starts-with","$key","uploads/"],["eq","$Content-Type","text/plain"],{"x-amz-meta-a":"1"}"#;
        assert_eq!(check(all, &fields, 0), Ok(None), "file, policy, x-amz-signature and x-ignore-* need no condition");
        assert_eq!(check(r#"["starts-with","$KEY","uploads/"],["EQ","$content-type","text/plain"],["starts-with","$X-Amz-Meta-A",""]"#, &fields, 0), Ok(None), "names and operators are case-insensitive");
        assert_eq!(check(&format!(r#"{all},["content-length-range",0,5]"#), &fields, 0), Ok(Some((0, 5))));
        // A simple condition is one name and value.
        assert_eq!(check(&format!("{all},{{}}"), &fields, 0), Err(Code::InvalidPolicyDocument));
        assert_eq!(check(r#"{"bucket":"b","key":"uploads/a.txt"}"#, &fields, 0), Err(Code::InvalidPolicyDocument));
        let exp = time::parse_iso8601("2030-01-01T00:00:00Z").unwrap();
        assert_eq!(check(all, &fields, exp), Ok(None));
        let denied = Err(Code::AccessDenied);
        assert_eq!(check(all, &fields, exp + 1), denied, "expired");
        assert_eq!(check(r#"{"bucket":"other"},["starts-with","$key",""],["starts-with","$content-type",""],{"x-amz-meta-a":"1"}"#, &fields, 0), denied);
        assert_eq!(check(r#"["starts-with","$key","other/"],["starts-with","$content-type",""],{"x-amz-meta-a":"1"}"#, &fields, 0), denied);
        assert_eq!(check(r#"["starts-with","$key",""],["eq","$content-type","text/html"],{"x-amz-meta-a":"1"}"#, &fields, 0), denied);
        assert_eq!(check(r#"["starts-with","$key",""],["starts-with","$content-type",""]"#, &fields, 0), denied, "x-amz-meta-a has no condition");
        assert_eq!(check(&format!(r#"{all},["starts-with","$missing","x"]"#), &fields, 0), denied);
        assert_eq!(check(&format!(r#"{all},["starts-with","$missing",""]"#), &fields, 0), Ok(None), "an empty prefix allows a missing field");
        assert_eq!(check(&format!(r#"{all},["eq","$missing",""]"#), &fields, 0), denied);
        let invalid = Err(Code::InvalidPolicyDocument);
        for bad in [r#"{"key":1}"#, r#"["in","$key","x"]"#, r#"["eq","key","x"]"#, r#"["eq","$key"]"#, r#"["content-length-range","a",10]"#, r#"["content-length-range",1]"#, r#""key""#, r#"[]"#, r#"[1,"$key","x"]"#] {
            assert_eq!(check(bad, &fields, 0), invalid, "{bad}");
        }
        let raw = |s: &str| check_post_policy(&crate::checksum::b64(s.as_bytes()), &fields, "b", 0).map(|p| p.content_length_range).map_err(|e| e.code);
        assert_eq!(raw(r#"{"expiration":"tomorrow","conditions":[]}"#), invalid);
        assert_eq!(raw(r#"{"expiration":5,"conditions":[]}"#), invalid);
        assert_eq!(raw(r#"{"expiration":"2030-01-01T00:00:00Z","conditions":{}}"#), invalid);
        assert_eq!(raw("not json"), invalid);
        assert_eq!(raw("[]"), invalid);
    }
}
