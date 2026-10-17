//! Request signing with [Standard Webhooks](https://www.standardwebhooks.com/): every request
//! carries `webhook-id`, `webhook-timestamp` and `webhook-signature: v1,<base64 hmac> …`, where
//! the HMAC-SHA256 covers `"{id}.{timestamp}.{body}"`. Requests are signed with both the current
//! and the next key, so receivers can rotate without missing a request. Any Standard Webhooks
//! library verifies them.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const PREFIX: &str = "whsec_";
/// Reject signatures this far from now (replay protection).
pub const TOLERANCE_S: i64 = 300;

pub fn new_secret() -> String {
    format!("{PREFIX}{}", B64.encode(crate::ids::token(32)))
}

fn key_bytes(secret: &str) -> Result<Vec<u8>, String> {
    B64.decode(secret.strip_prefix(PREFIX).unwrap_or(secret)).map_err(|e| format!("bad signing secret: {e}"))
}

pub fn sign(secret: &str, id: &str, timestamp: i64, body: &[u8]) -> Result<String, String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key_bytes(secret)?).map_err(|e| e.to_string())?;
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(format!("v1,{}", B64.encode(mac.finalize().into_bytes())))
}

/// The `webhook-signature` header value for a set of secrets.
pub fn signature_header(secrets: &[&str], id: &str, timestamp: i64, body: &[u8]) -> Result<String, String> {
    let sigs: Result<Vec<_>, _> = secrets.iter().map(|s| sign(s, id, timestamp, body)).collect();
    Ok(sigs?.join(" "))
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("missing webhook headers")]
    Missing,
    #[error("timestamp outside tolerance")]
    Expired,
    #[error("signature mismatch")]
    Mismatch,
}

/// Verify a signed request against a secret.
pub fn verify(secret: &str, id: &str, timestamp: &str, signature: &str, body: &[u8], now_s: i64) -> Result<(), VerifyError> {
    if id.is_empty() || signature.is_empty() {
        return Err(VerifyError::Missing);
    }
    let ts: i64 = timestamp.parse().map_err(|_| VerifyError::Missing)?;
    if (now_s - ts).abs() > TOLERANCE_S {
        return Err(VerifyError::Expired);
    }
    let expected = sign(secret, id, ts, body).map_err(|_| VerifyError::Mismatch)?;
    for candidate in signature.split(' ') {
        if constant_eq(candidate.as_bytes(), expected.as_bytes()) {
            return Ok(());
        }
    }
    Err(VerifyError::Mismatch)
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_webhooks_vector() {
        // Test vector from the Standard Webhooks reference implementations.
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let sig = sign(secret, "msg_p5jXN8AQM9LWM0D4loKWxJek", 1614265330, br#"{"test": 2432232314}"#).unwrap();
        assert_eq!(sig, "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
    }

    #[test]
    fn verify_accepts_any_listed_key() {
        let (a, b) = (new_secret(), new_secret());
        let now = 1_700_000_000;
        let header = signature_header(&[&a, &b], "msg_1", now, b"body").unwrap();
        assert_eq!(verify(&a, "msg_1", &now.to_string(), &header, b"body", now), Ok(()));
        assert_eq!(verify(&b, "msg_1", &now.to_string(), &header, b"body", now + 10), Ok(()));
        assert_eq!(verify(&new_secret(), "msg_1", &now.to_string(), &header, b"body", now), Err(VerifyError::Mismatch));
        assert_eq!(verify(&a, "msg_1", &now.to_string(), &header, b"tampered", now), Err(VerifyError::Mismatch));
        assert_eq!(verify(&a, "msg_2", &now.to_string(), &header, b"body", now), Err(VerifyError::Mismatch));
        assert_eq!(verify(&a, "msg_1", &now.to_string(), &header, b"body", now + 301), Err(VerifyError::Expired));
        assert_eq!(verify(&a, "", "1", &header, b"body", now), Err(VerifyError::Missing));
    }
}
