//! Envelope encryption (docs/secrets.md, "Encryption"): the deployment key (`ROLL_SECRET_KEY`)
//! derives a key-encryption key, which seals each project's random data key; the data key
//! encrypts every value with the row it belongs to as associated data.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

/// A secret value: redacted in `Debug`, zeroized on drop. Only [`Value::expose`] reads it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Value(Zeroizing<String>);

impl Value {
    pub fn new(s: impl Into<String>) -> Value {
        Value(Zeroizing::new(s.into()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Value(<redacted>)")
    }
}

impl serde::Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.expose())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("ROLL_SECRET_KEY must be at least 32 characters")]
    WeakKey,
    #[error("a project's data key doesn't open with ROLL_SECRET_KEY (or ROLL_SECRET_KEY_PREVIOUS)")]
    Dek,
    #[error("a stored value doesn't decrypt")]
    Open,
}

fn cipher(key: &[u8]) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key).expect("a 32-byte key")
}

fn seal_raw(c: &Aes256Gcm, aad: &str, msg: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).expect("system randomness");
    let ct = c.encrypt(Nonce::from_slice(&nonce), Payload { msg, aad: aad.as_bytes() }).expect("encrypt");
    let mut out = nonce.to_vec();
    out.extend(ct);
    out
}

fn open_raw(c: &Aes256Gcm, aad: &str, raw: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    if raw.len() < 12 + 16 {
        return Err(CryptoError::Open);
    }
    let (nonce, ct) = raw.split_at(12);
    c.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: aad.as_bytes() }).map(Zeroizing::new).map_err(|_| CryptoError::Open)
}

/// The key-encryption keys: the current one, and the previous one while data keys are resealed.
#[derive(Clone)]
pub struct Keyring {
    current: Aes256Gcm,
    previous: Option<Aes256Gcm>,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keyring")
    }
}

fn kek(secret: &str) -> Result<Aes256Gcm, CryptoError> {
    let secret = secret.trim();
    if secret.len() < 32 {
        return Err(CryptoError::WeakKey);
    }
    let mut okm = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<Sha256>::new(Some(b"roll-secrets"), secret.as_bytes()).expand(b"kek:v1", okm.as_mut()).expect("32 bytes");
    Ok(cipher(okm.as_ref()))
}

impl Keyring {
    pub fn new(current: &str, previous: Option<&str>) -> Result<Keyring, CryptoError> {
        Ok(Keyring { current: kek(current)?, previous: previous.filter(|p| !p.trim().is_empty()).map(kek).transpose()? })
    }

    /// A fresh data key for `project`, and its sealed form to store.
    pub fn new_dek(&self, project: &str) -> (Dek, String) {
        let mut raw = Zeroizing::new([0u8; 32]);
        getrandom::fill(raw.as_mut()).expect("system randomness");
        let sealed = self.seal_dek(project, raw.as_ref());
        (Dek::from_raw(raw.as_ref()), sealed)
    }

    fn seal_dek(&self, project: &str, raw: &[u8]) -> String {
        format!("v1:{}", STANDARD.encode(seal_raw(&self.current, &dek_aad(project), raw)))
    }

    /// Open a stored data key. The second value is the data key resealed with the current key,
    /// when it only opened with the previous one (store it).
    pub fn open_dek(&self, project: &str, sealed: &str) -> Result<(Dek, Option<String>), CryptoError> {
        let raw = sealed.strip_prefix("v1:").and_then(|s| STANDARD.decode(s).ok()).ok_or(CryptoError::Dek)?;
        let aad = dek_aad(project);
        if let Ok(k) = open_raw(&self.current, &aad, &raw) {
            return Ok((Dek::from_raw(&k), None));
        }
        let prev = self.previous.as_ref().ok_or(CryptoError::Dek)?;
        let k = open_raw(prev, &aad, &raw).map_err(|_| CryptoError::Dek)?;
        Ok((Dek::from_raw(&k), Some(self.seal_dek(project, &k))))
    }
}

fn dek_aad(project: &str) -> String {
    format!("roll-secrets:dek:{project}")
}

/// A project's data key, with the fingerprint key derived from it.
#[derive(Clone)]
pub struct Dek {
    cipher: Aes256Gcm,
    fp_key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for Dek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dek")
    }
}

impl Dek {
    fn from_raw(raw: &[u8]) -> Dek {
        let mut fp_key = Zeroizing::new([0u8; 32]);
        hkdf::Hkdf::<Sha256>::new(None, raw).expand(b"roll-secrets:fp:v1", fp_key.as_mut()).expect("32 bytes");
        Dek { cipher: cipher(raw), fp_key }
    }

    pub fn seal(&self, aad: &str, value: &Value) -> Vec<u8> {
        seal_raw(&self.cipher, aad, value.expose().as_bytes())
    }

    pub fn open(&self, aad: &str, sealed: &[u8]) -> Result<Value, CryptoError> {
        let pt = open_raw(&self.cipher, aad, sealed)?;
        String::from_utf8(pt.to_vec()).map(Value::new).map_err(|_| CryptoError::Open)
    }

    /// The first 8 bytes of HMAC-SHA256(fingerprint key, value), as hex: equal values have equal
    /// fingerprints inside one project, and nothing else can be learnt from one.
    pub fn fingerprint(&self, value: &str) -> String {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(self.fp_key.as_ref()).expect("any key length");
        m.update(value.as_bytes());
        m.finalize().into_bytes()[..8].iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The associated data of a value: its project, secret and version.
pub fn value_aad(project: &str, secret: &str, version: i64) -> String {
    format!("roll-secrets:v1:{project}:{secret}:{version}")
}

/// The associated data of a value pending in a change request.
pub fn request_aad(request: &str, n: usize) -> String {
    format!("roll-secrets:request:{request}:{n}")
}

/// A `roll-seal-v1` box (docs/code/agents.md, "Endpoints"): X25519 with a fresh key pair, HKDF
/// with salt `epk ‖ recipient`, AES-256-GCM with `context` as the associated data. The recipient
/// is a base64url X25519 public key.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Sealed {
    pub v: u8,
    pub epk: String,
    pub nonce: String,
    pub ct: String,
}

pub fn seal_to(recipient: &str, context: &str, plain: &[u8]) -> Result<Sealed, String> {
    use curve25519_dalek::montgomery::MontgomeryPoint;
    let public: [u8; 32] = URL_SAFE_NO_PAD
        .decode(recipient.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("seal_to is a base64url X25519 public key (32 bytes)")?;
    let mut esk = Zeroizing::new([0u8; 32]);
    getrandom::fill(esk.as_mut()).expect("system randomness");
    let epk = MontgomeryPoint::mul_base_clamped(*esk).to_bytes();
    let shared = Zeroizing::new(MontgomeryPoint(public).mul_clamped(*esk).to_bytes());
    if shared.iter().all(|b| *b == 0) {
        return Err("seal_to isn't a usable public key".into());
    }
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(&epk);
    salt[32..].copy_from_slice(&public);
    let mut okm = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<Sha256>::new(Some(&salt), shared.as_ref()).expand(b"roll-seal-v1", okm.as_mut()).expect("32 bytes");
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).expect("system randomness");
    let ct = cipher(okm.as_ref()).encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: context.as_bytes() }).expect("encrypt");
    Ok(Sealed { v: 1, epk: URL_SAFE_NO_PAD.encode(epk), nonce: URL_SAFE_NO_PAD.encode(nonce), ct: URL_SAFE_NO_PAD.encode(ct) })
}

/// Open a `roll-seal-v1` box with the recipient's secret key (tests and in-process consumers).
pub fn open_sealed(secret: &[u8; 32], context: &str, b: &Sealed) -> Result<Zeroizing<Vec<u8>>, String> {
    use curve25519_dalek::montgomery::MontgomeryPoint;
    let bad = || "the box doesn't open".to_string();
    let epk: [u8; 32] = URL_SAFE_NO_PAD.decode(&b.epk).ok().and_then(|v| v.try_into().ok()).ok_or_else(bad)?;
    let nonce: [u8; 12] = URL_SAFE_NO_PAD.decode(&b.nonce).ok().and_then(|v| v.try_into().ok()).ok_or_else(bad)?;
    let ct = URL_SAFE_NO_PAD.decode(&b.ct).map_err(|_| bad())?;
    let public = MontgomeryPoint::mul_base_clamped(*secret).to_bytes();
    let shared = Zeroizing::new(MontgomeryPoint(epk).mul_clamped(*secret).to_bytes());
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(&epk);
    salt[32..].copy_from_slice(&public);
    let mut okm = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<Sha256>::new(Some(&salt), shared.as_ref()).expand(b"roll-seal-v1", okm.as_mut()).expect("32 bytes");
    cipher(okm.as_ref()).decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: context.as_bytes() }).map(Zeroizing::new).map_err(|_| bad())
}

/// The public key of an X25519 secret key, base64url (tests and in-process consumers).
pub fn public_key(secret: &[u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped(*secret).to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_bound_to_their_row_and_keys_rotate() {
        assert!(Keyring::new("short", None).is_err());
        let old = Keyring::new(&"a".repeat(32), None).unwrap();
        let (dek, sealed) = old.new_dek("prj_a");
        let ct = dek.seal(&value_aad("prj_a", "sec_1", 1), &Value::new("hunter2"));
        assert_eq!(dek.open(&value_aad("prj_a", "sec_1", 1), &ct).unwrap().expose(), "hunter2");
        assert!(dek.open(&value_aad("prj_a", "sec_1", 2), &ct).is_err(), "another version");
        assert!(dek.open(&value_aad("prj_b", "sec_1", 1), &ct).is_err(), "another project");
        assert!(old.open_dek("prj_b", &sealed).is_err(), "a data key is bound to its project");
        // A new deployment key opens the data key through the previous one and reseals it.
        let new = Keyring::new(&"b".repeat(32), Some(&"a".repeat(32))).unwrap();
        let (dek2, resealed) = new.open_dek("prj_a", &sealed).unwrap();
        let resealed = resealed.expect("resealed with the new key");
        assert_eq!(dek2.open(&value_aad("prj_a", "sec_1", 1), &ct).unwrap().expose(), "hunter2");
        assert!(Keyring::new(&"b".repeat(32), None).unwrap().open_dek("prj_a", &resealed).unwrap().1.is_none());
        assert_eq!(dek.fingerprint("x"), dek2.fingerprint("x"));
        assert_ne!(dek.fingerprint("x"), dek.fingerprint("y"));
        assert_eq!(format!("{:?}", Value::new("hunter2")), "Value(<redacted>)");
    }

    #[test]
    fn sealed_boxes_open_only_for_their_recipient_and_context() {
        let sk = [7u8; 32];
        let b = seal_to(&public_key(&sk), "ctx", b"A=1").unwrap();
        assert_eq!(&*open_sealed(&sk, "ctx", &b).unwrap(), b"A=1");
        assert!(open_sealed(&sk, "other", &b).is_err());
        assert!(open_sealed(&[8u8; 32], "ctx", &b).is_err());
    }
}
