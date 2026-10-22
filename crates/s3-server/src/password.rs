//! IAM users' console passwords: hashing (argon2id) and the account's password policy.
//!
//! - A password is only ever kept as an argon2id PHC string (19 MiB, 2 passes, 1 lane: OWASP's
//!   recommendation) with a random 16-byte salt. Passwords and hashes are never logged, and no
//!   error echoes one.
//! - Hashing runs on the blocking pool, so a sign-in doesn't stall the request threads.
//! - The policy is IAM's: `UpdateAccountPasswordPolicy`'s rules when the account has one, else
//!   AWS's default policy (at least 8 characters, three of uppercase, lowercase, numbers and
//!   symbols, not the account's name or email address). Passwords are printable ASCII, at most
//!   128 characters.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use s3_store::iam::PasswordPolicy;

pub(crate) const MAX_LEN: usize = 128;
/// IAM's symbols.
const SYMBOLS: &str = "!@#$%^&*()_+-=[]{}|'";

fn hasher() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(19_456, 2, 1, None).expect("valid argon2 parameters"))
}

/// Hash a password (on the blocking pool).
pub(crate) async fn hash(password: &str) -> Result<String, String> {
    let pw = zeroize::Zeroizing::new(password.to_string());
    tokio::task::spawn_blocking(move || {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| format!("randomness: {e}"))?;
        let salt = SaltString::encode_b64(&salt).map_err(|e| e.to_string())?;
        hasher().hash_password(pw.as_bytes(), &salt).map(|h| h.to_string()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whether `password` is the one `hash` was made from (on the blocking pool). A hash that
/// doesn't parse matches nothing.
pub(crate) async fn verify(password: &str, hash: &str) -> bool {
    let pw = zeroize::Zeroizing::new(password.to_string());
    let hash = hash.to_string();
    tokio::task::spawn_blocking(move || PasswordHash::new(&hash).is_ok_and(|h| hasher().verify_password(pw.as_bytes(), &h).is_ok())).await.unwrap_or(false)
}

/// Whether `password` is any of `hashes` (`PasswordReusePrevention`).
pub(crate) async fn reused(password: &str, hashes: &[String]) -> bool {
    for h in hashes {
        if verify(password, h).await {
            return true;
        }
    }
    false
}

/// Check a new password against the account's policy (`None`: AWS's default policy).
/// `names`: what the default policy forbids (the account's name and email address). `Err` is
/// IAM's `PasswordPolicyViolation` message.
pub(crate) fn check(policy: Option<&PasswordPolicy>, password: &str, names: &[&str]) -> Result<(), String> {
    if password.is_empty() || password.chars().count() > MAX_LEN {
        return Err(format!("Password should have a length between 1 and {MAX_LEN} characters"));
    }
    if !password.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err("Password should contain only printable Basic Latin (ASCII) characters".into());
    }
    let upper = password.chars().any(|c| c.is_ascii_uppercase());
    let lower = password.chars().any(|c| c.is_ascii_lowercase());
    let digit = password.chars().any(|c| c.is_ascii_digit());
    let symbol = password.chars().any(|c| SYMBOLS.contains(c));
    match policy {
        Some(p) => {
            if (password.len() as u32) < p.min_length {
                return Err(format!("Password should have a minimum length of {}", p.min_length));
            }
            if p.require_uppercase && !upper {
                return Err("Password should have at least one uppercase letter".into());
            }
            if p.require_lowercase && !lower {
                return Err("Password should have at least one lowercase letter".into());
            }
            if p.require_numbers && !digit {
                return Err("Password should have at least one number".into());
            }
            if p.require_symbols && !symbol {
                return Err(format!("Password should have at least one symbol ({SYMBOLS})"));
            }
        }
        None => {
            if password.len() < 8 {
                return Err("Password should have a minimum length of 8".into());
            }
            if [upper, lower, digit, symbol].iter().filter(|x| **x).count() < 3 {
                return Err("Password should have at least three of: uppercase letters, lowercase letters, numbers and symbols".into());
            }
            if names.iter().any(|n| !n.is_empty() && password.eq_ignore_ascii_case(n)) {
                return Err("Password should not be the account's name or email address".into());
            }
        }
    }
    Ok(())
}

/// Whether a password set at `changed_at` has expired under `policy` at `now` (Unix ms).
pub(crate) fn expired(policy: Option<&PasswordPolicy>, changed_at: i64, now: i64) -> bool {
    policy.and_then(|p| p.max_age_days).is_some_and(|d| now - changed_at >= i64::from(d) * 86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_verify_and_are_salted() {
        let h = hash("correct horse Battery 9").await.unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{h}");
        assert!(!h.contains("correct"));
        assert!(verify("correct horse Battery 9", &h).await);
        assert!(!verify("correct horse battery 9", &h).await);
        assert!(!verify("x", "not a hash").await);
        assert_ne!(hash("same").await.unwrap(), hash("same").await.unwrap(), "salted");
        assert!(reused("same", &[h.clone(), hash("same").await.unwrap()]).await);
        assert!(!reused("other", &[h]).await);
    }

    #[test]
    fn policies() {
        // AWS's default policy.
        assert!(check(None, "Abcdefg1", &[]).is_ok());
        assert!(check(None, "abcdefg1", &[]).unwrap_err().contains("three"));
        assert!(check(None, "Abc1!", &[]).unwrap_err().contains("minimum length of 8"));
        assert!(check(None, "Default1x", &["default1x"]).unwrap_err().contains("account's name"));
        assert!(check(None, "Pässword1!", &[]).unwrap_err().contains("ASCII"));
        assert!(check(None, &"Aa1!".repeat(33), &[]).is_err());
        // An account's own policy.
        let p = PasswordPolicy { min_length: 10, require_symbols: true, require_numbers: true, require_uppercase: true, require_lowercase: true, ..Default::default() };
        assert!(check(Some(&p), "Abcdefgh1!", &[]).is_ok());
        assert_eq!(check(Some(&p), "Abcdefg1!", &[]).unwrap_err(), "Password should have a minimum length of 10");
        assert!(check(Some(&p), "abcdefgh1!", &[]).unwrap_err().contains("uppercase"));
        assert!(check(Some(&p), "ABCDEFGH1!", &[]).unwrap_err().contains("lowercase"));
        assert!(check(Some(&p), "Abcdefghi!", &[]).unwrap_err().contains("number"));
        assert!(check(Some(&p), "Abcdefghi1", &[]).unwrap_err().contains("symbol"));
        assert!(check(Some(&PasswordPolicy::default()), "abcdef", &[]).is_ok(), "the defaults of UpdateAccountPasswordPolicy: 6 characters");
        let aging = PasswordPolicy { max_age_days: Some(30), ..Default::default() };
        assert!(!expired(Some(&aging), 0, 29 * 86_400_000));
        assert!(expired(Some(&aging), 0, 30 * 86_400_000));
        assert!(!expired(None, 0, i64::MAX));
    }
}
