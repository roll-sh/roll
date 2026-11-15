//! The `roll-seal-v1` box (docs/code/agents.md, "Endpoints"; roll-secrets' `crypto::seal_to`), on
//! the receiving side: roll secrets backups are sealed to a key only this machine holds.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use curve25519_dalek::montgomery::MontgomeryPoint;
use sha2::Sha256;

/// A fresh X25519 key pair: the secret key and the public key, both base64url.
pub fn keypair() -> (String, String) {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("system randomness");
    let public = MontgomeryPoint::mul_base_clamped(secret).to_bytes();
    let out = (URL_SAFE_NO_PAD.encode(secret), URL_SAFE_NO_PAD.encode(public));
    secret.fill(0);
    out
}

/// Open a box (`{v, epk, nonce, ct}`) with the secret key, `context` as the associated data.
pub fn open(secret_b64: &str, context: &str, sealed: &serde_json::Value) -> anyhow::Result<Vec<u8>> {
    let field = |k: &str| sealed[k].as_str().and_then(|s| URL_SAFE_NO_PAD.decode(s).ok()).ok_or_else(|| anyhow::anyhow!("not a roll-seal-v1 box"));
    let secret: [u8; 32] = URL_SAFE_NO_PAD.decode(secret_b64.trim()).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| anyhow::anyhow!("the key file isn't a backup secret key (roll secrets keygen)"))?;
    let epk: [u8; 32] = field("epk")?.try_into().map_err(|_| anyhow::anyhow!("not a roll-seal-v1 box"))?;
    let nonce: [u8; 12] = field("nonce")?.try_into().map_err(|_| anyhow::anyhow!("not a roll-seal-v1 box"))?;
    let ct = field("ct")?;
    let public = MontgomeryPoint::mul_base_clamped(secret).to_bytes();
    let shared = MontgomeryPoint(epk).mul_clamped(secret).to_bytes();
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(&epk);
    salt[32..].copy_from_slice(&public);
    let mut okm = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(Some(&salt), &shared).expand(b"roll-seal-v1", &mut okm).expect("32 bytes");
    let plain = Aes256Gcm::new_from_slice(&okm).expect("a 32-byte key").decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: context.as_bytes() });
    okm.fill(0);
    plain.map_err(|_| anyhow::anyhow!("this key doesn't open that backup"))
}
