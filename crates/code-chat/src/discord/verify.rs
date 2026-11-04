//! Interaction signing: Discord signs `<X-Signature-Timestamp><raw body>` with Ed25519 under the
//! application's key and sends the signature, hex, in `X-Signature-Ed25519`. Discord itself
//! doesn't bound the timestamp; we refuse one more than five minutes off, so a captured request
//! can't be replayed later.

use crate::{MAX_SKEW_S, VerifyError};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

/// Check an interaction's `X-Signature-Ed25519` and `X-Signature-Timestamp` against its raw
/// body with the application's public key (hex).
pub fn verify(
    public_key_hex: &str,
    signature_hex: Option<&str>,
    timestamp: Option<&str>,
    body: &[u8],
    now_unix: i64,
) -> Result<(), VerifyError> {
    let (Some(signature), Some(timestamp)) = (signature_hex, timestamp) else {
        return Err(VerifyError::Missing);
    };
    let ts: i64 = timestamp
        .trim()
        .parse()
        .map_err(|_| VerifyError::Mismatch)?;
    if now_unix.abs_diff(ts) > MAX_SKEW_S as u64 {
        return Err(VerifyError::Stale);
    }
    let key = hex::decode(public_key_hex.trim()).map_err(|_| VerifyError::Mismatch)?;
    let signature = hex::decode(signature.trim()).map_err(|_| VerifyError::Mismatch)?;
    let mut message = Vec::with_capacity(timestamp.len() + body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.extend_from_slice(body);
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&message, &signature)
        .map_err(|_| VerifyError::Mismatch)
}

/// Signs requests the way Discord does, from a fixed seed: for tests and mock servers in this
/// and other crates. Never a production key.
#[doc(hidden)]
pub struct TestSigner {
    key: Ed25519KeyPair,
}

impl TestSigner {
    pub fn new(seed: [u8; 32]) -> TestSigner {
        TestSigner {
            key: Ed25519KeyPair::from_seed_unchecked(&seed).expect("any 32 bytes are a seed"),
        }
    }

    /// The application public key to configure, hex.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.key.public_key().as_ref())
    }

    /// The `X-Signature-Ed25519` for `body` sent with `X-Signature-Timestamp: <timestamp>`.
    pub fn sign(&self, timestamp: &str, body: &[u8]) -> String {
        let mut message = timestamp.as_bytes().to_vec();
        message.extend_from_slice(body);
        hex::encode(self.key.sign(&message).as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_760_000_000;

    #[test]
    fn discord_round_trip() {
        let signer = TestSigner::new([7; 32]);
        let key = signer.public_key_hex();
        let ts = NOW.to_string();
        let body = br#"{"type":1}"#;
        let sig = signer.sign(&ts, body);
        assert_eq!(verify(&key, Some(&sig), Some(&ts), body, NOW + 299), Ok(()));
        assert_eq!(
            verify(
                &key.to_uppercase(),
                Some(&sig.to_uppercase()),
                Some(&ts),
                body,
                NOW
            ),
            Ok(())
        );
    }

    #[test]
    fn discord_rejections() {
        let signer = TestSigner::new([7; 32]);
        let key = signer.public_key_hex();
        let ts = NOW.to_string();
        let body = br#"{"type":1}"#;
        let sig = signer.sign(&ts, body);
        assert_eq!(
            verify(&key, Some(&sig), Some(&ts), br#"{"type":2}"#, NOW),
            Err(VerifyError::Mismatch)
        );
        // The timestamp is signed.
        let other = (NOW + 1).to_string();
        assert_eq!(
            verify(&key, Some(&sig), Some(&other), body, NOW),
            Err(VerifyError::Mismatch)
        );
        let stranger = TestSigner::new([8; 32]).public_key_hex();
        assert_eq!(
            verify(&stranger, Some(&sig), Some(&ts), body, NOW),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(&key, Some("zz"), Some(&ts), body, NOW),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify("nothex", Some(&sig), Some(&ts), body, NOW),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(&key, Some(&sig), Some(&ts), body, NOW + 301),
            Err(VerifyError::Stale)
        );
        assert_eq!(
            verify(&key, Some(&sig), Some(&ts), body, NOW - 301),
            Err(VerifyError::Stale)
        );
        assert_eq!(
            verify(&key, None, Some(&ts), body, NOW),
            Err(VerifyError::Missing)
        );
        assert_eq!(
            verify(&key, Some(&sig), None, body, NOW),
            Err(VerifyError::Missing)
        );
    }
}
