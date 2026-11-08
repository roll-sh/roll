//! Certificates' private keys and the ACME account key at rest: AES-256-GCM under a key derived
//! from `ROLL_SECRET_KEY`, with the object's key as associated data (a sealed object copied under
//! another key doesn't open). Stored as `rse1` ‖ nonce ‖ ciphertext.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"rse1";

#[derive(Clone)]
pub struct Sealer {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer")
    }
}

impl Sealer {
    /// `secret`: `ROLL_SECRET_KEY`, at least 32 characters.
    pub fn new(secret: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(secret.trim().len() >= 32, "ROLL_SECRET_KEY must be at least 32 characters");
        let key = Sha256::digest(format!("roll-edge-certs:{}", secret.trim()).as_bytes());
        Ok(Self { cipher: Aes256Gcm::new_from_slice(&key).expect("32-byte key") })
    }

    pub fn seal(&self, object_key: &str, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).expect("system randomness");
        let ct = self.cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: object_key.as_bytes() }).expect("encrypt");
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&nonce);
        out.extend(ct);
        out
    }

    pub fn open(&self, object_key: &str, sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        let rest = sealed.strip_prefix(MAGIC).ok_or_else(|| anyhow::anyhow!("{object_key} isn't sealed"))?;
        anyhow::ensure!(rest.len() >= 12 + 16, "{object_key} is truncated");
        let (nonce, ct) = rest.split_at(12);
        self.cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: object_key.as_bytes() })
            .map_err(|_| anyhow::anyhow!("{object_key} doesn't open with this ROLL_SECRET_KEY"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_binding() {
        assert!(Sealer::new("short").is_err());
        let s = Sealer::new("0123456789abcdef0123456789abcdef").unwrap();
        let sealed = s.seal("system/certs/cloud.sealed", b"-----BEGIN PRIVATE KEY-----");
        assert!(!sealed.windows(5).any(|w| w == b"BEGIN"));
        assert_eq!(s.open("system/certs/cloud.sealed", &sealed).unwrap(), b"-----BEGIN PRIVATE KEY-----");
        assert!(s.open("system/certs/site.sealed", &sealed).is_err(), "bound to its object key");
        assert!(Sealer::new("fedcba9876543210fedcba9876543210").unwrap().open("system/certs/cloud.sealed", &sealed).is_err());
    }
}
