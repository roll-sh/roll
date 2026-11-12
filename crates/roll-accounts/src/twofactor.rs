//! Codes and second factors: email codes hashed with argon2id, TOTP seeds sealed under
//! `ROLL_SECRET_KEY`, recovery codes hashed with SHA-256 (80 random bits each), and the
//! durable count of wrong codes.
//!
//! - A TOTP code is accepted for the current step or one either side (`roll_auth::totp`), and
//!   only if its step is newer than the last one accepted: the step is moved forward by a
//!   compare-and-set, so a code works once even when two nodes see it at the same time.
//! - Ten wrong TOTP or recovery codes per user within five minutes refuse every code until the
//!   window passes. Each try is counted before its code is checked, in one write.

use crate::{Accounts, Error, MINUTE, Result, random, sha256_hex};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use code_sql::{OptionalExtension, params};
use roll_auth::totp;
use sha2::{Digest, Sha256};

/// Wrong second-factor codes a user gets per window.
const FAILS: i64 = 10;
const FAIL_WINDOW_MS: i64 = 5 * MINUTE;
const RECOVERY_CODES: usize = 10;

fn hasher() -> Argon2<'static> {
    // 19 MiB, 2 passes, 1 lane: OWASP's recommendation (as storage's console passwords).
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(19_456, 2, 1, None).expect("valid argon2 parameters"))
}

/// An email code as stored: an argon2id PHC string with a random salt.
pub(crate) fn hash_code(code: &str) -> Result<String> {
    let salt = SaltString::encode_b64(&random::<16>()).map_err(|e| Error::Internal(e.to_string()))?;
    hasher().hash_password(code.as_bytes(), &salt).map(|h| h.to_string()).map_err(|e| Error::Internal(e.to_string()))
}

pub(crate) fn verify_code(code: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| hasher().verify_password(code.as_bytes(), &h).is_ok())
}

/// AES-256-GCM under a key derived from `ROLL_SECRET_KEY`, the user's id as associated data (a
/// seed copied to another user doesn't open). Stored as `v1:` + base64(nonce ‖ ciphertext).
struct Sealer(Aes256Gcm);

impl Sealer {
    fn new(secret: &str) -> Sealer {
        let key = Sha256::digest(format!("roll-accounts-totp:{}", secret.trim()).as_bytes());
        Sealer(Aes256Gcm::new_from_slice(&key).expect("32-byte key"))
    }

    fn seal(&self, user: &str, plain: &str) -> String {
        let nonce = random::<12>();
        let ct = self.0.encrypt(Nonce::from_slice(&nonce), Payload { msg: plain.as_bytes(), aad: user.as_bytes() }).expect("encrypt");
        format!("v1:{}", base64::engine::general_purpose::STANDARD.encode([&nonce[..], &ct].concat()))
    }

    fn open(&self, user: &str, sealed: &str) -> Option<zeroize::Zeroizing<String>> {
        let raw = base64::engine::general_purpose::STANDARD.decode(sealed.strip_prefix("v1:")?).ok()?;
        if raw.len() < 12 + 16 {
            return None;
        }
        let (nonce, ct) = raw.split_at(12);
        let plain = self.0.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: user.as_bytes() }).ok()?;
        String::from_utf8(plain).ok().map(zeroize::Zeroizing::new)
    }
}

/// A new TOTP seed, before it's confirmed.
pub struct Enrolment {
    pub secret: String,
    pub otpauth_url: String,
    /// A PNG of the QR code, as a `data:` URL.
    pub qr_png: String,
}

/// `xxxx-xxxx-xxxx-xxxx` (10 random bytes in Base32), and what we keep of it.
fn recovery_code() -> (String, String) {
    let raw = totp::base32_encode(&random::<10>()).to_ascii_lowercase();
    let shown = raw.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).expect("ascii")).collect::<Vec<_>>().join("-");
    (shown, sha256_hex(&raw))
}

fn recovery_hash(code: &str) -> String {
    let raw: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    sha256_hex(&raw)
}

impl Accounts {
    fn sealer(&self) -> Result<Sealer> {
        match &self.config.secret_key {
            Some(k) if k.expose().len() >= 32 => Ok(Sealer::new(k.expose())),
            _ => Err(Error::Unavailable("authenticator apps need ROLL_SECRET_KEY (at least 32 characters) on the server".into(), "secret_key")),
        }
    }

    /// A new seed for `user`'s authenticator app, off until [`Accounts::totp_confirm`].
    pub fn totp_enrol(&self, user: &str) -> Result<Enrolment> {
        let sealer = self.sealer()?;
        let email = self.user(user)?.email;
        let secret = totp::new_seed();
        let sealed = sealer.seal(user, &secret);
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM totp WHERE user_id = ? AND enabled = 1", [user], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("an authenticator app is already on: turn it off first".into()));
            }
            c.execute(
                "INSERT INTO totp (user_id, seed, enabled, last_step, created_at) VALUES (?, ?, 0, 0, ?) ON CONFLICT (user_id) DO UPDATE SET seed = excluded.seed, last_step = 0, created_at = excluded.created_at",
                params![user, sealed, now],
            )?;
            Ok(())
        })?;
        let otpauth_url = totp::otpauth_uri("roll", &format!("roll:{email}"), &secret);
        let png = totp::qr_png(&otpauth_url).unwrap_or_default();
        let qr_png = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png));
        Ok(Enrolment { secret, otpauth_url, qr_png })
    }

    /// Turn the pending seed on with a code from it: the new recovery codes, shown once.
    pub fn totp_confirm(&self, user: &str, code: &str) -> Result<Vec<String>> {
        // The seed shown (sealed): only it is turned on, so a second setup started in another tab
        // meanwhile can't turn on a seed the person never saw.
        let Some(seed) = self.db.read(|c| c.query_row("SELECT seed FROM totp WHERE user_id = ? AND enabled = 0", [user], |r| r.get::<_, String>(0)).optional())? else {
            return Err(Error::NotFound("no authenticator app is waiting to be confirmed".into()));
        };
        if !self.check_totp(user, code, false)? {
            return Err(Error::bad_code());
        }
        if self.db.write(|c| c.execute("UPDATE totp SET enabled = 1 WHERE user_id = ? AND enabled = 0 AND seed = ?", [user, seed.as_str()]))? == 0 {
            return Err(Error::Conflict("the authenticator app's setup changed meanwhile: start it again".into()));
        }
        self.new_recovery_codes(user)
    }

    /// Turn the authenticator app off, with a code from it or a recovery code.
    pub fn totp_disable(&self, user: &str, code: &str) -> Result<()> {
        if !self.second_factor(user, code)? {
            return Err(Error::bad_code());
        }
        self.db.write(|c| {
            c.execute("DELETE FROM totp WHERE user_id = ?", [user])?;
            c.execute("DELETE FROM recovery_codes WHERE user_id = ?", [user])?;
            Ok(())
        })
    }

    /// New recovery codes (the old ones stop working), with a code from the app.
    pub fn regenerate_recovery_codes(&self, user: &str, code: &str) -> Result<Vec<String>> {
        if !self.check_totp(user, code, true)? {
            return Err(Error::bad_code());
        }
        self.new_recovery_codes(user)
    }

    fn new_recovery_codes(&self, user: &str) -> Result<Vec<String>> {
        let codes: Vec<(String, String)> = (0..RECOVERY_CODES).map(|_| recovery_code()).collect();
        self.db.write(|c| {
            c.execute("DELETE FROM recovery_codes WHERE user_id = ?", [user])?;
            for (_, hash) in &codes {
                c.execute("INSERT INTO recovery_codes (user_id, code_hash) VALUES (?, ?)", [user, hash.as_str()])?;
            }
            Ok::<_, Error>(())
        })?;
        Ok(codes.into_iter().map(|(shown, _)| shown).collect())
    }

    /// A TOTP code or a recovery code (used up).
    pub(crate) fn second_factor(&self, user: &str, code: &str) -> Result<bool> {
        if totp::parse_code(code).is_some() { self.check_totp(user, code, true) } else { self.check_recovery(user, code) }
    }

    /// Check a TOTP code from `user`'s seed (`enabled`: the confirmed one, else the pending
    /// one) and use its step up. Wrong codes count against the user.
    pub(crate) fn check_totp(&self, user: &str, code: &str, enabled: bool) -> Result<bool> {
        let key = format!("2fa:{user}");
        if !self.attempt(&key, FAILS, FAIL_WINDOW_MS)? {
            return Ok(false);
        }
        let row = self.db.read(|c| c.query_row("SELECT seed, last_step FROM totp WHERE user_id = ? AND enabled = ?", params![user, enabled], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).optional())?;
        let Some((sealed, last)) = row else { return Ok(false) };
        let seed = self.sealer()?.open(user, &sealed).ok_or_else(|| Error::Internal("the TOTP seed can't be opened (was ROLL_SECRET_KEY changed?)".into()))?;
        let now_s = self.db.now_ms()?.div_euclid(1000);
        let step = totp::parse_code(code).and_then(|n| totp::step_of(&totp::base32_decode(&seed).unwrap_or_default(), n, now_s)).filter(|s| *s > last);
        let used = match step {
            Some(s) => self.db.write(|c| c.execute("UPDATE totp SET last_step = ? WHERE user_id = ? AND last_step < ?", params![s, user, s]))? == 1,
            None => false,
        };
        if used {
            self.forget(&key)?;
        }
        Ok(used)
    }

    pub(crate) fn check_recovery(&self, user: &str, code: &str) -> Result<bool> {
        let key = format!("2fa:{user}");
        if !self.attempt(&key, FAILS, FAIL_WINDOW_MS)? {
            return Ok(false);
        }
        let hash = recovery_hash(code);
        let used = self.db.write(|c| c.execute("DELETE FROM recovery_codes WHERE user_id = ? AND code_hash = ?", [user, hash.as_str()]))? == 1;
        if used {
            self.forget(&key)?;
        }
        Ok(used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_bind_the_user_and_codes_hash() {
        let s = Sealer::new("0123456789abcdef0123456789abcdef");
        let sealed = s.seal("usr_a", "SEED");
        assert!(!sealed.contains("SEED"));
        assert_eq!(s.open("usr_a", &sealed).as_deref().map(String::as_str), Some("SEED"));
        assert!(s.open("usr_b", &sealed).is_none() && Sealer::new("another key, 32 characters long!!").open("usr_a", &sealed).is_none());
        let h = hash_code("123456").unwrap();
        assert!(verify_code("123456", &h) && !verify_code("123457", &h));
        let (shown, kept) = recovery_code();
        assert_eq!(shown.len(), 19);
        assert_eq!(recovery_hash(&shown.to_uppercase()), kept);
    }
}
