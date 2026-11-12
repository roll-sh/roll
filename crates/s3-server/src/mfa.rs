//! Virtual MFA devices: TOTP (RFC 6238, the math in `roll_auth::totp`), their Base32 seeds and
//! QR codes, and checking a code once.
//!
//! - A code is accepted for the current step or the one before or after it (clock drift), and
//!   only if its step is newer than the last one accepted for the device: a code can't be
//!   replayed, and an older code can't be used after a newer one. The accepted step is written
//!   durably before the request goes on.
//! - Seeds are 20 random bytes (160 bits, RFC 4226's recommendation), shown once as Base32 and
//!   as an `otpauth://` QR code. They're never logged.
//! - Ten wrong codes within five minutes lock a device for the rest of those five minutes. The
//!   count is durable (it survives a restart and holds across a takeover) and costs successful
//!   checks no write.

use s3_store::Store;
use s3_store::iam::MfaDevice;

pub(crate) use roll_auth::totp::{base32_decode, consecutive, new_seed, parse_code, qr_png};
use roll_auth::totp::step_of;

/// Wrong codes a device takes within [`FAIL_WINDOW_MS`] before it refuses every code until the
/// window passes (guessing six digits is then hopeless).
pub(crate) const MAX_FAILS: u32 = 10;
pub(crate) const FAIL_WINDOW_MS: i64 = 300_000;

/// What the throttle counts a device's wrong codes under (the device's own key).
fn throttle_key(account: &str, serial: &str) -> Option<String> {
    s3_store::iam::mfa_name_of(account, serial).map(|n| format!("mfa:{}", n.to_lowercase()))
}

/// Check `code` from the enabled device `serial` of `account` and use it up. `owner` says
/// whose it must be: `None` the root user's, `Some(user)` that IAM user's. Any failure (an
/// unknown device, someone else's, a wrong or reused code, too many wrong codes lately) is
/// `false`.
///
/// Wrong codes are counted in the metadata (`s3-store`'s `throttle.rs`), so the throttle
/// survives a restart and holds across a takeover; counting is a relaxed write, and a right
/// code only reads unless there were wrong ones to forget.
pub(crate) async fn check(store: &Store, account: &str, serial: &str, code: &str, owner: Option<&str>) -> Result<bool, s3_store::StoreError> {
    let now_s = store.now().div_euclid(1000);
    let Some(key) = throttle_key(account, serial) else { return Ok(false) };
    if store.throttled(account, &key, MAX_FAILS, FAIL_WINDOW_MS)? {
        return Ok(false);
    }
    match check_once(store, account, serial, code, owner, now_s).await? {
        Some(true) => {
            store.clear_failures(account, &key)?;
            Ok(true)
        }
        // Only a device that exists counts wrong codes (nobody can grow the table with made-up
        // serial numbers).
        Some(false) => {
            store.record_failure(account, &key, FAIL_WINDOW_MS)?;
            Ok(false)
        }
        None => Ok(false),
    }
}

/// `None`: no such enabled device of `owner`.
async fn check_once(store: &Store, account: &str, serial: &str, code: &str, owner: Option<&str>, now_s: i64) -> Result<Option<bool>, s3_store::StoreError> {
    let Some(d) = store.mfa_device(account, serial)? else { return Ok(None) };
    if !belongs(&d, owner) {
        return Ok(None);
    }
    let Some(code) = parse_code(code) else { return Ok(Some(false)) };
    let Some(seed) = base32_decode(&d.seed) else { return Ok(Some(false)) };
    let Some(step) = step_of(&seed, code, now_s) else { return Ok(Some(false)) };
    store.use_mfa_step(account, serial, step).await.map(Some)
}

fn belongs(d: &MfaDevice, owner: Option<&str>) -> bool {
    d.enabled_at.is_some()
        && match owner {
            None => d.root,
            Some(u) => !d.root && d.user.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(u)),
        }
}

/// `otpauth://` for authenticator apps.
pub(crate) fn otpauth_uri(account: &str, user: &str, seed: &str) -> String {
    roll_auth::totp::otpauth_uri("roll", &format!("roll:{user}@{account}"), seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_keys() {
        assert_eq!(throttle_key("123456789012", "arn:aws:iam::123456789012:mfa/Team/Alice").as_deref(), Some("mfa:team/alice"));
        assert_eq!(throttle_key("123456789012", "arn:aws:iam::999999999999:mfa/alice"), None);
    }
}
