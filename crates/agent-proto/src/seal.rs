//! The `roll-seal-v1` box ([`SealedSecret`]): sealing to a daemon's `seal_key`, as the dashboard
//! does in the browser (`apps/dashboard/src/lib/agents-seal.ts`) and roll's relay does for an
//! endpoint's `roll://` key (`endpoint.key`). Only the daemon opens it (agent-host's `SealKey`).

use crate::{SEAL_VERSION, SealedSecret};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use curve25519_dalek::montgomery::MontgomeryPoint;
use sha2::Sha256;
use zeroize::Zeroizing;

/// The HKDF info of a `roll-seal-v1` box.
const SEAL_INFO: &[u8] = b"roll-seal-v1";

/// The box's AES-256-GCM key: HKDF-SHA256 of the X25519 secret, salted with `epk ‖ public`.
pub fn box_cipher(shared: &[u8; 32], epk: &[u8; 32], public: &[u8; 32]) -> aes_gcm::Aes256Gcm {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(epk);
    salt[32..].copy_from_slice(public);
    let mut okm = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<Sha256>::new(Some(&salt), shared).expand(SEAL_INFO, okm.as_mut()).expect("32 bytes is a valid HKDF length");
    aes_gcm::Aes256Gcm::new_from_slice(okm.as_ref()).expect("a 32-byte key")
}

/// Seals `key` to the daemon whose public key is `public` (base64url), with `context` as the
/// associated data (what the key is for: [`SealedSecret::context`], [`SealedSecret::ref_context`],
/// a gateway upstream's), so a box made for one never opens as another. The error never holds
/// the key.
pub fn seal_with(public: &str, context: &str, key: &str) -> Result<SealedSecret, String> {
    let public: [u8; 32] = B64.decode(public).ok().and_then(|b| b.try_into().ok()).ok_or("a seal key is 32 bytes, base64url")?;
    let esk = Zeroizing::new(rand::random::<[u8; 32]>());
    let epk = MontgomeryPoint::mul_base_clamped(*esk).to_bytes();
    let shared = Zeroizing::new(MontgomeryPoint(public).mul_clamped(*esk).to_bytes());
    // A low-order point gives an all-zero secret anyone could compute.
    if shared.iter().all(|b| *b == 0) {
        return Err("that isn't a daemon's seal key".into());
    }
    let nonce: [u8; 12] = rand::random();
    let ct = box_cipher(&shared, &epk, &public)
        .encrypt(&nonce.into(), Payload { msg: key.as_bytes(), aad: context.as_bytes() })
        .map_err(|_| "sealing failed".to_string())?;
    Ok(SealedSecret { v: SEAL_VERSION, epk: B64.encode(epk), nonce: B64.encode(nonce), ct: B64.encode(ct) })
}
