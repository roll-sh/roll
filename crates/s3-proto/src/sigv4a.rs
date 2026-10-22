//! AWS Signature Version 4a (`AWS4-ECDSA-P256-SHA256`): SigV4's canonical request, signed with
//! an ECDSA P-256 key derived from the secret, valid for a set of regions
//! (`x-amz-region-set`) instead of one. Multi-Region Access Points and the CRT-based clients
//! use it.
//!
//! - The key is derived from the access key and secret with NIST SP 800-108 HMAC-SHA256 in
//!   counter mode ([`derive_key`]). It's constant for a key pair, so the server caches the
//!   public half per access key.
//! - The credential scope has no region: `<date>/<service>/aws4_request`. Any region set is
//!   accepted (one deployment serves every region name, as SigV4's scope region is).
//! - Signatures are DER-encoded and hex. ECDSA signatures are randomised, so they're verified,
//!   never recomputed and compared.
//! - Signed `aws-chunked` bodies (`STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD[-TRAILER]`) chain
//!   chunk signatures from the seed signature like SigV4's. Clients may pad chunk signatures to
//!   144 characters with `*`; the padding is never part of what's signed.

use crate::error::{Code, S3Error};
use crate::sigv4::{self, Credential, Payload, Request, Signed, StreamSigning, Verified};
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{DerSignature, Signature, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

pub const ALGORITHM: &str = "AWS4-ECDSA-P256-SHA256";

/// The P-256 group order minus two, big-endian (the KDF's bound).
const N_MINUS_2: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63, 0x25, 0x4f,
];

/// The ECDSA key of an access key pair (FIPS 186-5 A.2.2 with the KDF of NIST SP 800-108):
/// `k0 = HMAC-SHA256("AWS4A" + secret, 1u32 ‖ "AWS4-ECDSA-P256-SHA256" ‖ 0x00 ‖ access key ‖
/// counter ‖ 256u32)`, retried with the next counter until `k0 ≤ n − 2`; the private scalar is
/// `k0 + 1`.
pub fn derive_key(access_key: &str, secret: &str) -> SigningKey {
    let input_key = format!("AWS4A{secret}");
    for counter in 1u8..=254 {
        let mut fixed = Vec::with_capacity(4 + ALGORITHM.len() + 1 + access_key.len() + 1 + 4);
        fixed.extend_from_slice(&1u32.to_be_bytes());
        fixed.extend_from_slice(ALGORITHM.as_bytes());
        fixed.push(0);
        fixed.extend_from_slice(access_key.as_bytes());
        fixed.push(counter);
        fixed.extend_from_slice(&256u32.to_be_bytes());
        let mut mac = Hmac::<Sha256>::new_from_slice(input_key.as_bytes()).expect("hmac takes any key length");
        mac.update(&fixed);
        let k0: [u8; 32] = mac.finalize().into_bytes().into();
        // Big-endian byte strings of equal length compare like the numbers they encode.
        if k0 <= N_MINUS_2 {
            let mut d = k0;
            for b in d.iter_mut().rev() {
                let (v, carry) = b.overflowing_add(1);
                *b = v;
                if !carry {
                    break;
                }
            }
            return SigningKey::from_bytes(&d.into()).expect("1 ≤ k0 + 1 ≤ n − 1 is a valid scalar");
        }
    }
    // Each try fails with probability ~2^-32; 254 failures in a row don't happen.
    unreachable!("SigV4a key derivation exhausted its counter")
}

/// The public key the server verifies with.
pub fn verifying_key(access_key: &str, secret: &str) -> VerifyingKey {
    *derive_key(access_key, secret).verifying_key()
}

/// `<date>/<service>/aws4_request`.
pub fn scope(c: &Credential) -> String {
    format!("{}/{}/aws4_request", c.date, c.service)
}

/// `<access key>/<yyyymmdd>/<service>/aws4_request` (`service` must be `expect_service`).
fn parse_credential(s: &str, code: Code, expect_service: &str) -> Result<Credential, S3Error> {
    let parts: Vec<&str> = s.trim().split('/').collect();
    let bad = |why: &str| S3Error::msg(code, format!("Error parsing the X-Amz-Credential parameter; {why}"));
    if parts.len() != 4 || parts[3] != "aws4_request" {
        return Err(bad("the Credential is mal-formed; expecting \"<YOUR-AKID>/YYYYMMDD/SERVICE/aws4_request\"."));
    }
    if parts[0].is_empty() {
        return Err(bad("the access key is empty."));
    }
    if parts[1].len() != 8 || !parts[1].bytes().all(|c| c.is_ascii_digit()) {
        return Err(bad(&format!("incorrect date format \"{}\". This date in the credential must be in the format \"yyyyMMdd\".", parts[1])));
    }
    if parts[2] != expect_service {
        return Err(bad(&format!("incorrect service \"{}\". This endpoint belongs to \"{expect_service}\".", parts[2])));
    }
    Ok(Credential { access_key: parts[0].into(), date: parts[1].into(), region: String::new(), service: parts[2].into() })
}

/// A SigV4a request, parsed (not yet verified).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedV4a {
    /// The SigV4 parts (the credential's `region` is empty: the scope has none).
    pub signed: Signed,
    /// `x-amz-region-set`, as sent (every region is accepted).
    pub region_set: String,
}

/// Parse a SigV4a request (`None` if it isn't one). Doesn't check the clock or the signature.
pub fn parse(req: &Request) -> Result<Option<SignedV4a>, S3Error> {
    parse_for(req, "s3")
}

pub(crate) fn parse_for(req: &Request, service: &str) -> Result<Option<SignedV4a>, S3Error> {
    if let Some(auth) = req.header("authorization") {
        let Some(rest) = auth.trim().strip_prefix(ALGORITHM) else { return Ok(None) };
        let (cred, signed_headers, signature) = sigv4::components(rest)?;
        let credential = parse_credential(cred, Code::AuthorizationHeaderMalformed, service)?;
        let (amz_date, timestamp) = sigv4::request_time(req)?;
        if amz_date[..8] != credential.date {
            return Err(sigv4::malformed(format!("The authorization header is malformed; Invalid credential date \"{}\". This date is not the same as X-Amz-Date: \"{}\".", credential.date, &amz_date[..8])));
        }
        let region_set = req.header("x-amz-region-set").map(str::trim).filter(|r| !r.is_empty()).ok_or_else(|| S3Error::msg(Code::InvalidRequest, "Missing required header for this request: x-amz-region-set"))?;
        let payload = Payload::parse(req.header("x-amz-content-sha256").ok_or_else(|| S3Error::msg(Code::InvalidRequest, "Missing required header for this request: x-amz-content-sha256"))?)?;
        if matches!(payload, Payload::Streaming { .. }) {
            return Err(S3Error::msg(Code::InvalidRequest, "STREAMING-AWS4-HMAC-SHA256-PAYLOAD needs an AWS4-HMAC-SHA256 signature.").with("Header", "x-amz-content-sha256"));
        }
        let signature = signature.trim_end_matches('*').to_string();
        let signed = Signed { credential, signed_headers, signature, amz_date, timestamp, payload, presigned_expires: None };
        return Ok(Some(SignedV4a { signed, region_set: region_set.to_string() }));
    }
    let query = crate::http::parse_query(req.query);
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if q("X-Amz-Algorithm") != Some(ALGORITHM) {
        return Ok(None);
    }
    let missing = || S3Error::msg(Code::AuthorizationQueryParametersError, "Query-string authentication with AWS4-ECDSA-P256-SHA256 requires the X-Amz-Algorithm, X-Amz-Credential, X-Amz-Region-Set, X-Amz-Signature, X-Amz-Date, X-Amz-SignedHeaders, and X-Amz-Expires parameters.");
    let credential = parse_credential(q("X-Amz-Credential").ok_or_else(missing)?, Code::AuthorizationQueryParametersError, service)?;
    let region_set = q("X-Amz-Region-Set").filter(|r| !r.trim().is_empty()).ok_or_else(missing)?.to_string();
    let mut signed = sigv4::presigned_rest(&query, &credential.date)?;
    signed.credential = credential;
    signed.signature = signed.signature.trim_end_matches('*').to_string();
    Ok(Some(SignedV4a { signed, region_set }))
}

pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!("{ALGORITHM}\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical_request.as_bytes())))
}

/// Whether `signature_hex` (DER, hex) is `key`'s signature of `string_to_sign`.
pub fn verify_signature(key: &VerifyingKey, string_to_sign: &str, signature_hex: &str) -> bool {
    let Ok(der) = hex::decode(signature_hex.trim_end_matches('*')) else { return false };
    let Ok(sig) = Signature::from_der(&der) else { return false };
    // Either `s` is a valid signature; some signers don't normalise to the low one.
    let sig = sig.normalize_s().unwrap_or(sig);
    key.verify(string_to_sign.as_bytes(), &sig).is_ok()
}

/// Sign (clients and tests): DER, hex.
pub fn sign(key: &SigningKey, string_to_sign: &str) -> String {
    let sig: DerSignature = key.sign(string_to_sign.as_bytes());
    hex::encode(sig.as_bytes())
}

/// Check the signature with the access key's public key (from [`verifying_key`]).
pub fn verify(req: &Request, s: &SignedV4a, key: &VerifyingKey) -> Result<Verified, S3Error> {
    let signed = &s.signed;
    for h in &signed.signed_headers {
        if req.header(h).is_none() {
            return Err(S3Error::msg(Code::SignatureDoesNotMatch, format!("Signed header {h} is missing from the request")));
        }
    }
    let canonical = sigv4::canonical_request(req, signed);
    let scope = scope(&signed.credential);
    let sts = string_to_sign(&signed.amz_date, &scope, &canonical);
    if !verify_signature(key, &sts, &signed.signature) {
        return Err(S3Error::new(Code::SignatureDoesNotMatch)
            .with("AWSAccessKeyId", signed.credential.access_key.clone())
            .with("StringToSign", sts)
            .with("SignatureProvided", signed.signature.clone())
            .with("CanonicalRequest", canonical));
    }
    let stream = matches!(signed.payload, Payload::StreamingEcdsa { .. })
        .then(|| StreamSigning { key: sigv4::ChunkKey::Ecdsa(*key), seed_signature: signed.signature.clone(), amz_date: signed.amz_date.clone(), scope });
    Ok(Verified { access_key: signed.credential.access_key.clone(), payload: signed.payload.clone(), stream })
}

/// Everything in one call: `Ok(None)` if the request isn't SigV4a-signed. `lookup` maps an
/// access key to its secret; `key` gives the (cacheable) public key of an access key and its
/// secret, normally [`verifying_key`].
pub fn authenticate(req: &Request, now: i64, lookup: impl FnOnce(&str) -> Option<String>, key: impl FnOnce(&str, &str) -> VerifyingKey) -> Result<Option<Verified>, S3Error> {
    let Some(s) = parse(req)? else { return Ok(None) };
    let ak = s.signed.credential.access_key.clone();
    let secret = lookup(&ak).ok_or_else(|| S3Error::new(Code::InvalidAccessKeyId).with("AWSAccessKeyId", ak.clone()))?;
    sigv4::check_time(&s.signed, now)?;
    verify(req, &s, &key(&ak, &secret)).map(Some)
}

fn chunk_sts(amz_date: &str, scope: &str, previous: &str, chunk_sha256_hex: &str) -> String {
    format!("{ALGORITHM}-PAYLOAD\n{amz_date}\n{scope}\n{}\n{}\n{chunk_sha256_hex}", previous.trim_end_matches('*'), sigv4::EMPTY_SHA256)
}

fn trailer_sts(amz_date: &str, scope: &str, previous: &str, trailers: &[(String, String)]) -> String {
    let mut canon = String::new();
    for (k, v) in trailers {
        canon.push_str(k);
        canon.push(':');
        canon.push_str(v);
        canon.push('\n');
    }
    format!("{ALGORITHM}-TRAILER\n{amz_date}\n{scope}\n{}\n{}", previous.trim_end_matches('*'), hex::encode(Sha256::digest(canon.as_bytes())))
}

pub(crate) fn verify_chunk(key: &VerifyingKey, amz_date: &str, scope: &str, previous: &str, chunk_sha256_hex: &str, signature: &str) -> bool {
    verify_signature(key, &chunk_sts(amz_date, scope, previous, chunk_sha256_hex), signature)
}

pub(crate) fn verify_trailer(key: &VerifyingKey, amz_date: &str, scope: &str, previous: &str, trailers: &[(String, String)], signature: &str) -> bool {
    verify_signature(key, &trailer_sts(amz_date, scope, previous, trailers), signature)
}

/// The signature of one chunk (clients and tests); `pad` pads it to 144 characters with `*`,
/// as the CRT clients do.
pub fn chunk_signature(key: &SigningKey, amz_date: &str, scope: &str, previous: &str, chunk_sha256_hex: &str, pad: bool) -> String {
    padded(sign(key, &chunk_sts(amz_date, scope, previous, chunk_sha256_hex)), pad)
}

pub fn trailer_signature(key: &SigningKey, amz_date: &str, scope: &str, previous: &str, trailers: &[(String, String)], pad: bool) -> String {
    padded(sign(key, &trailer_sts(amz_date, scope, previous, trailers)), pad)
}

fn padded(mut s: String, pad: bool) -> String {
    if pad {
        while s.len() < 144 {
            s.push('*');
        }
    }
    s
}

/// Encode a body as SigV4a-signed `aws-chunked` (clients and tests).
#[allow(clippy::too_many_arguments)]
pub fn encode_chunked(payload: &[u8], chunk: usize, key: &SigningKey, seed: &str, amz_date: &str, scope: &str, trailers: &[(String, String)], pad: bool) -> Vec<u8> {
    crate::chunked::encode_with(payload, chunk, Some(seed), trailers, |prev, hash| chunk_signature(key, amz_date, scope, prev, hash, pad), |prev, t| trailer_signature(key, amz_date, scope, prev, t, pad))
}

/// Sign a request (clients and tests): every given header is signed, and must include `host`,
/// `x-amz-date`, `x-amz-region-set` and `x-amz-content-sha256`. Returns the `Authorization`
/// value; the signature is the request's seed signature for a chunked body.
pub fn authorization(method: &str, raw_path: &str, raw_query: &str, headers: &[(&str, &str)], access_key: &str, secret: &str) -> String {
    let req = Request { method, path: raw_path, query: raw_query, headers };
    let amz_date = req.header("x-amz-date").unwrap_or_default().to_string();
    let date = amz_date.get(..8).unwrap_or_default().to_string();
    let mut names: Vec<String> = headers.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
    names.sort();
    names.dedup();
    let payload = req.header("x-amz-content-sha256").and_then(|v| Payload::parse(v).ok()).unwrap_or(Payload::Unsigned);
    let credential = Credential { access_key: access_key.into(), date: date.clone(), region: String::new(), service: "s3".into() };
    let s = Signed { credential, signed_headers: names.clone(), signature: String::new(), amz_date: amz_date.clone(), timestamp: 0, payload, presigned_expires: None };
    let sc = scope(&s.credential);
    let sig = sign(&derive_key(access_key, secret), &string_to_sign(&amz_date, &sc, &sigv4::canonical_request(&req, &s)));
    format!("{ALGORITHM} Credential={access_key}/{sc}, SignedHeaders={}, Signature={sig}", names.join(";"))
}

/// A presigned URL's query string (host-only signed headers, unsigned payload), for clients and
/// tests: `path` is the raw (encoded) path.
#[allow(clippy::too_many_arguments)]
pub fn presign_query(method: &str, host: &str, path: &str, region_set: &str, access_key: &str, secret: &str, now: i64, expires_s: i64) -> String {
    use crate::http::uri_encode;
    let amz_date = crate::time::amz_date(now);
    let date = &amz_date[..8];
    let params: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".into(), ALGORITHM.into()),
        ("X-Amz-Credential".into(), format!("{access_key}/{date}/s3/aws4_request")),
        ("X-Amz-Date".into(), amz_date.clone()),
        ("X-Amz-Expires".into(), expires_s.to_string()),
        ("X-Amz-Region-Set".into(), region_set.into()),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    let query = params.iter().map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true))).collect::<Vec<_>>().join("&");
    let headers = [("host", host)];
    let req = Request { method, path, query: &query, headers: &headers };
    let credential = Credential { access_key: access_key.into(), date: date.into(), region: String::new(), service: "s3".into() };
    let s = Signed { credential, signed_headers: vec!["host".into()], signature: String::new(), amz_date: amz_date.clone(), timestamp: now, payload: Payload::Unsigned, presigned_expires: Some(expires_s) };
    let sig = sign(&derive_key(access_key, secret), &string_to_sign(&amz_date, &scope(&s.credential), &sigv4::canonical_request(&req, &s)));
    format!("{query}&X-Amz-Signature={sig}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunked::{Decoder, Mode};
    use bytes::Bytes;

    // From aws-sdk-rust's SigV4a signing test suite (`aws-sigv4/aws-signing-test-suite/v4a`).
    const AK: &str = "AKIDEXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const PUBLIC_X: &str = "b6618f6a65740a99e650b33b6b4b5bd0d43b176d721a3edfea7e7d2d56d936b1";
    const PUBLIC_Y: &str = "865ed22a7eadc9c5cb9d2cbaca1b3699139fedc5043dc6661864218330c8e518";
    const T: i64 = 1_440_938_160; // 20150830T123600Z

    #[test]
    fn derives_the_suites_public_key() {
        let vk = verifying_key(AK, SK);
        let point = vk.to_encoded_point(false);
        assert_eq!(hex::encode(point.x().unwrap()), PUBLIC_X);
        assert_eq!(hex::encode(point.y().unwrap()), PUBLIC_Y);
        // Another pair derives another key; the same pair always the same one.
        assert_ne!(verifying_key("AKIDEXAMPLF", SK), vk);
        assert_eq!(verifying_key(AK, SK), vk);
    }

    #[test]
    fn suite_get_vanilla_header() {
        let canonical = "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\nx-amz-region-set:us-east-1\n\nhost;x-amz-date;x-amz-region-set\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let sts = string_to_sign("20150830T123600Z", "20150830/service/aws4_request", canonical);
        assert_eq!(sts, "AWS4-ECDSA-P256-SHA256\n20150830T123600Z\n20150830/service/aws4_request\ncf59db423e841c8b7e3444158185aa261b724a5c27cbe762676f3eed19f4dc02");
        let vk = verifying_key(AK, SK);
        // Both signatures the suite lists for this request.
        for sig in [
            "3045022018b4e277d0281864beb51d3600e23f88510ea5031d68ddfbb68614b82a5eb7d2022100effb9c5f22ed9ef3ae0ab243d21f06bce82365bbb79529a07b6888c343ae5f8c",
            "3046022100fddaaf816a31e30d04973875b13bc27b98da10907a3a1872fd5819a221334abc022100de2ee81d83b54f31efc82834de07e6cd4d5c6f25bfa08e0f29a15bb0977c343e",
        ] {
            assert!(verify_signature(&vk, &sts, sig), "{sig}");
        }
        // And through the request parser: the suite's request, with the payload hash S3 needs.
        let auth = "AWS4-ECDSA-P256-SHA256 Credential=AKIDEXAMPLE/20150830/service/aws4_request, SignedHeaders=host;x-amz-date;x-amz-region-set, Signature=3046022100fddaaf816a31e30d04973875b13bc27b98da10907a3a1872fd5819a221334abc022100de2ee81d83b54f31efc82834de07e6cd4d5c6f25bfa08e0f29a15bb0977c343e";
        let headers = [("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z"), ("X-Amz-Region-Set", "us-east-1"), ("x-amz-content-sha256", sigv4::EMPTY_SHA256), ("Authorization", auth)];
        let req = Request { method: "GET", path: "/", query: "", headers: &headers };
        let s = parse_for(&req, "service").unwrap().unwrap();
        assert_eq!((s.region_set.as_str(), s.signed.credential.date.as_str()), ("us-east-1", "20150830"));
        assert_eq!(sigv4::canonical_request(&req, &s.signed), canonical);
        let v = verify(&req, &s, &vk).unwrap();
        assert_eq!(v.access_key, AK);
        assert!(v.stream.is_none());
        // The wrong key, or any change to what's signed, fails.
        assert_eq!(verify(&req, &s, &verifying_key(AK, "other")).unwrap_err().code, Code::SignatureDoesNotMatch);
        let moved = [("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z"), ("X-Amz-Region-Set", "us-west-2"), ("x-amz-content-sha256", sigv4::EMPTY_SHA256), ("Authorization", auth)];
        assert_eq!(verify(&Request { headers: &moved, ..req }, &s, &vk).unwrap_err().code, Code::SignatureDoesNotMatch, "the region set is signed");
        // S3 requests need the s3 service.
        assert_eq!(parse(&req).unwrap_err().code, Code::AuthorizationHeaderMalformed);
    }

    #[test]
    fn suite_get_vanilla_query() {
        let canonical = "GET\n/\nX-Amz-Algorithm=AWS4-ECDSA-P256-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20150830%2Fservice%2Faws4_request&X-Amz-Date=20150830T123600Z&X-Amz-Expires=3600&X-Amz-Region-Set=us-east-1&X-Amz-SignedHeaders=host\nhost:example.amazonaws.com\n\nhost\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let sts = string_to_sign("20150830T123600Z", "20150830/service/aws4_request", canonical);
        assert_eq!(sts, "AWS4-ECDSA-P256-SHA256\n20150830T123600Z\n20150830/service/aws4_request\n890c4ed28c1a1ac10b5862719b537afbe392e987dc1aab1efa16fe7de41d3c81");
        let vk = verifying_key(AK, SK);
        for sig in [
            "3045022100b3246f8e1442dd58e73292ae1294720c3256ee58f6cea50f3a3f83b4a4b0331a0220329693f7745c9008a5887c0529ceccb0fae4e16d707b712ea66acdbaf963769a",
            "3045022100a28466db6c76223d5aa6ed436a8451b30e21ba0a076e7f2f943dd1f3d2816249022049e4b3746929cd2a2cb7b7984dbe0f5d40a300e7d9a7a7f1307c196b6f96b61a",
        ] {
            assert!(verify_signature(&vk, &sts, sig), "{sig}");
        }
        // Through the parser, with the payload hash the suite signed as X-Amz-Content-Sha256.
        let query = "X-Amz-Algorithm=AWS4-ECDSA-P256-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20150830%2Fservice%2Faws4_request&X-Amz-Date=20150830T123600Z&X-Amz-SignedHeaders=host&X-Amz-Expires=3600&X-Amz-Region-Set=us-east-1&X-Amz-Signature=3045022100a28466db6c76223d5aa6ed436a8451b30e21ba0a076e7f2f943dd1f3d2816249022049e4b3746929cd2a2cb7b7984dbe0f5d40a300e7d9a7a7f1307c196b6f96b61a";
        let headers = [("Host", "example.amazonaws.com")];
        let req = Request { method: "GET", path: "/", query, headers: &headers };
        let mut s = parse_for(&req, "service").unwrap().unwrap();
        assert_eq!(s.signed.presigned_expires, Some(3600));
        s.signed.payload = Payload::Sha256(sigv4::EMPTY_SHA256.into());
        verify(&req, &s, &vk).unwrap();
        sigv4::check_time(&s.signed, T + 3600).unwrap();
        assert_eq!(sigv4::check_time(&s.signed, T + 3601).unwrap_err().code, Code::AccessDenied);
    }

    #[test]
    fn signs_and_verifies_s3_requests() {
        let headers = [("host", "127.0.0.1:59000"), ("x-amz-date", "20150830T123600Z"), ("x-amz-region-set", "*"), ("x-amz-content-sha256", sigv4::UNSIGNED_PAYLOAD), ("x-amz-meta-a", "1")];
        let auth = authorization("PUT", "/b/a%20k", "x=1", &headers, AK, SK);
        assert!(auth.starts_with("AWS4-ECDSA-P256-SHA256 Credential=AKIDEXAMPLE/20150830/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-meta-a;x-amz-region-set, Signature=30"), "{auth}");
        let mut with = headers.to_vec();
        with.push(("authorization", auth.as_str()));
        let req = Request { method: "PUT", path: "/b/a%20k", query: "x=1", headers: &with };
        let ok = |r: &Request, now: i64, secret: &str| authenticate(r, now, |_| Some(secret.to_string()), verifying_key).map_err(|e| e.code);
        let v = ok(&req, T, SK).unwrap().unwrap();
        assert_eq!((v.access_key.as_str(), v.payload.clone()), (AK, Payload::Unsigned));
        assert_eq!(ok(&req, T, "wrong"), Err(Code::SignatureDoesNotMatch));
        assert_eq!(ok(&req, T + 3600, SK), Err(Code::RequestTimeTooSkewed));
        assert_eq!(authenticate(&req, T, |_| None, verifying_key).unwrap_err().code, Code::InvalidAccessKeyId);
        assert_eq!(ok(&Request { query: "x=2", ..req }, T, SK), Err(Code::SignatureDoesNotMatch));
        // Not SigV4a: nothing to say.
        assert_eq!(parse(&Request { headers: &headers, ..req }).unwrap(), None);
        // No region set.
        let no_set: Vec<_> = with.iter().filter(|(k, _)| *k != "x-amz-region-set").copied().collect();
        assert_eq!(parse(&Request { headers: &no_set, ..req }).unwrap_err().code, Code::InvalidRequest);
        // A credential with a region in it is SigV4's shape, not SigV4a's.
        let v4_shape = [("host", "h"), ("x-amz-date", "20150830T123600Z"), ("x-amz-region-set", "*"), ("x-amz-content-sha256", sigv4::UNSIGNED_PAYLOAD), ("authorization", "AWS4-ECDSA-P256-SHA256 Credential=AK/20150830/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=30")];
        assert_eq!(parse(&Request { headers: &v4_shape, ..req }).unwrap_err().code, Code::AuthorizationHeaderMalformed);
        // HMAC-signed chunks under an ECDSA signature are refused.
        let hmac_chunks = [("host", "h"), ("x-amz-date", "20150830T123600Z"), ("x-amz-region-set", "*"), ("x-amz-content-sha256", sigv4::STREAMING_PAYLOAD), ("authorization", "AWS4-ECDSA-P256-SHA256 Credential=AK/20150830/s3/aws4_request, SignedHeaders=host, Signature=30")];
        assert_eq!(parse(&Request { headers: &hmac_chunks, ..req }).unwrap_err().code, Code::InvalidRequest);
    }

    #[test]
    fn presigned_round_trip() {
        let query = presign_query("GET", "127.0.0.1:59000", "/b/k%2Bx", "us-east-1,eu-*", AK, SK, T, 60);
        let headers = [("host", "127.0.0.1:59000")];
        let req = Request { method: "GET", path: "/b/k%2Bx", query: &query, headers: &headers };
        let v = authenticate(&req, T + 30, |_| Some(SK.into()), verifying_key).unwrap().unwrap();
        assert_eq!(v.payload, Payload::Unsigned);
        assert_eq!(authenticate(&req, T + 61, |_| Some(SK.into()), verifying_key).unwrap_err().code, Code::AccessDenied);
        assert_eq!(authenticate(&Request { method: "PUT", ..req }, T, |_| Some(SK.into()), verifying_key).unwrap_err().code, Code::SignatureDoesNotMatch);
        let without_set = query.replace("&X-Amz-Region-Set=us-east-1%2Ceu-%2A", "");
        assert_eq!(parse(&Request { query: &without_set, ..req }).unwrap_err().code, Code::AuthorizationQueryParametersError);
    }

    fn feed_all(d: &mut Decoder, body: &[u8], step: usize) -> Result<Vec<u8>, S3Error> {
        let mut pieces = Vec::new();
        for c in body.chunks(step) {
            d.feed(Bytes::copy_from_slice(c), &mut pieces)?;
        }
        Ok(pieces.iter().flat_map(|p| p.to_vec()).collect())
    }

    #[test]
    fn signed_chunks_with_and_without_padding() {
        let payload: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let headers = [("host", "h"), ("x-amz-date", "20150830T123600Z"), ("x-amz-region-set", "*"), ("x-amz-content-sha256", sigv4::STREAMING_ECDSA_PAYLOAD_TRAILER), ("x-amz-decoded-content-length", "70000")];
        let auth = authorization("PUT", "/b/k", "", &headers, AK, SK);
        let mut with = headers.to_vec();
        with.push(("authorization", auth.as_str()));
        let req = Request { method: "PUT", path: "/b/k", query: "", headers: &with };
        let v = authenticate(&req, T, |_| Some(SK.into()), verifying_key).unwrap().unwrap();
        let st = v.stream.clone().unwrap();
        assert!(st.signing_key().is_none());
        assert_eq!(st.scope, "20150830/s3/aws4_request");
        let key = derive_key(AK, SK);
        let crc = crate::checksum::b64(&crate::checksum::Algorithm::Crc32c.checksum(&payload));
        let trailers = vec![("x-amz-checksum-crc32c".to_string(), crc)];
        for pad in [false, true] {
            let body = encode_chunked(&payload, 16_384, &key, &st.seed_signature, &st.amz_date, &st.scope, &trailers, pad);
            assert_eq!(body.windows(3).any(|w| w == b"***"), pad);
            let mut d = Decoder::new(Mode::Signed { signing: st.clone(), trailer: true }, Some(payload.len() as u64));
            assert_eq!(feed_all(&mut d, &body, 1000).unwrap(), payload, "pad {pad}");
            assert_eq!(d.finish().unwrap(), trailers);
            // A flipped payload byte fails its chunk.
            let mut bad = body.clone();
            let at = bad.windows(4).position(|w| w == b"\r\n\x00\x01").unwrap() + 4;
            bad[at] ^= 1;
            let mut d = Decoder::new(Mode::Signed { signing: st.clone(), trailer: true }, Some(payload.len() as u64));
            assert_eq!(feed_all(&mut d, &bad, 1000).unwrap_err().code, Code::SignatureDoesNotMatch);
            // So do chunks signed by another key, and changed trailers.
            let other = encode_chunked(&payload, 16_384, &derive_key(AK, "other"), &st.seed_signature, &st.amz_date, &st.scope, &trailers, pad);
            let mut d = Decoder::new(Mode::Signed { signing: st.clone(), trailer: true }, Some(payload.len() as u64));
            assert_eq!(feed_all(&mut d, &other, 1000).unwrap_err().code, Code::SignatureDoesNotMatch);
            let wrong_crc = vec![("x-amz-checksum-crc32c".to_string(), "AAAAAA==".to_string())];
            let swapped = encode_chunked(&payload, 16_384, &key, &st.seed_signature, &st.amz_date, &st.scope, &wrong_crc, pad);
            let sig_at = swapped.windows(24).position(|w| w == b"x-amz-trailer-signature:").unwrap();
            let mut tampered = body[..body.windows(21).position(|w| w == b"x-amz-checksum-crc32c").unwrap()].to_vec();
            tampered.extend_from_slice(b"x-amz-checksum-crc32c:AAAAAA==\r\n");
            tampered.extend_from_slice(&body[body.windows(24).position(|w| w == b"x-amz-trailer-signature:").unwrap()..]);
            assert!(sig_at > 0);
            let mut d = Decoder::new(Mode::Signed { signing: st.clone(), trailer: true }, Some(payload.len() as u64));
            assert_eq!(feed_all(&mut d, &tampered, 1000).unwrap_err().code, Code::SignatureDoesNotMatch);
        }
    }
}
