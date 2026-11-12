//! Bring-your-own provider keys at rest: AES-256-GCM under a key derived from the server's
//! `ROLL_SECRET_KEY`, with the owner's id as associated data (a ciphertext copied to another
//! owner doesn't decrypt). Stored as `v1:` + base64(nonce ‖ ciphertext).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct Sealer {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("ROLL_SECRET_KEY must be at least 32 characters")]
    WeakKey,
    #[error("the stored key can't be decrypted (wrong ROLL_SECRET_KEY, or it belongs to another owner)")]
    Open,
}

impl Sealer {
    pub fn new(secret: &str) -> Result<Sealer, SecretError> {
        if secret.trim().len() < 32 {
            return Err(SecretError::WeakKey);
        }
        let key = Sha256::digest(format!("roll-review-keys:{}", secret.trim()).as_bytes());
        Ok(Sealer { cipher: Aes256Gcm::new_from_slice(&key).expect("32-byte key") })
    }

    pub fn seal(&self, owner_id: &str, plaintext: &str) -> String {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).expect("system randomness");
        let ct = self.cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext.as_bytes(), aad: owner_id.as_bytes() }).expect("encrypt");
        let mut out = nonce.to_vec();
        out.extend(ct);
        format!("v1:{}", base64::engine::general_purpose::STANDARD.encode(out))
    }

    pub fn open(&self, owner_id: &str, sealed: &str) -> Result<String, SecretError> {
        let raw = sealed.strip_prefix("v1:").ok_or(SecretError::Open)?;
        let raw = base64::engine::general_purpose::STANDARD.decode(raw).map_err(|_| SecretError::Open)?;
        if raw.len() < 12 + 16 {
            return Err(SecretError::Open);
        }
        let (nonce, ct) = raw.split_at(12);
        let pt = self.cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: owner_id.as_bytes() }).map_err(|_| SecretError::Open)?;
        String::from_utf8(pt).map_err(|_| SecretError::Open)
    }
}

/// The last 4 characters of a key, for display; nothing for a key so short that 4 characters
/// would give most of it away.
pub fn last4(key: &str) -> String {
    let k: Vec<char> = key.trim().chars().collect();
    if k.len() < 16 {
        return String::new();
    }
    k[k.len() - 4..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_binding() {
        assert!(Sealer::new("short").is_err());
        let s = Sealer::new("0123456789abcdef0123456789abcdef").unwrap();
        let sealed = s.seal("own_a", "sk-ant-api03-secret-value");
        assert!(sealed.starts_with("v1:") && !sealed.contains("secret-value"));
        assert_eq!(s.open("own_a", &sealed).unwrap(), "sk-ant-api03-secret-value");
        // Another owner, another server key, tampering: all fail.
        assert!(s.open("own_b", &sealed).is_err());
        let other = Sealer::new("ffffffffffffffffffffffffffffffff").unwrap();
        assert!(other.open("own_a", &sealed).is_err());
        let mut t = sealed.clone();
        t.pop();
        t.push('A');
        assert!(s.open("own_a", &t).is_err() || s.open("own_a", &t).unwrap() != "sk-ant-api03-secret-value");
        assert!(s.open("own_a", "garbage").is_err());
        // Two seals of the same key differ (random nonce).
        assert_ne!(s.seal("own_a", "k"), s.seal("own_a", "k"));
        assert_eq!(last4("sk-ant-api03-abcd1234"), "1234");
        assert_eq!(last4("sk-abcd1234"), "", "short keys show nothing");
        assert_eq!(last4("ab"), "");
        assert_eq!(format!("{s:?}"), "Sealer");
    }
}
