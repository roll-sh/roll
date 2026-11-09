//! Virtual MFA devices: TOTP (RFC 6238: HMAC-SHA1, 30 s steps, 6 digits), their Base32 seeds
//! and QR codes, and checking a code once.
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

/// Seconds per step.
pub(crate) const STEP_S: i64 = 30;
/// Steps either side of now a code may come from.
const DRIFT: i64 = 1;
/// How far back `EnableMFADevice`'s two consecutive codes may be (AWS resynchronizes over a few
/// minutes too).
const ENABLE_WINDOW: i64 = 10;

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub(crate) fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let (mut buf, mut bits) = (0u64, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub(crate) fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut buf, mut bits) = (0u64, 0u32);
    for c in s.trim().trim_end_matches('=').bytes() {
        let v = B32.iter().position(|x| *x == c.to_ascii_uppercase())? as u64;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// A new seed, Base32.
pub(crate) fn new_seed() -> String {
    let mut b = [0u8; 20];
    getrandom::fill(&mut b).expect("os randomness");
    base32_encode(&b)
}

/// The 6-digit code of `step`.
pub(crate) fn totp(seed: &[u8], step: i64) -> u32 {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, seed);
    let tag = ring::hmac::sign(&key, &(step as u64).to_be_bytes());
    let h = tag.as_ref();
    let o = (h[19] & 0x0f) as usize;
    let n = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]);
    n % 1_000_000
}

/// A 6-digit code as sent.
pub(crate) fn parse_code(code: &str) -> Option<u32> {
    let c = code.trim();
    (c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit())).then(|| c.parse().ok()).flatten()
}

/// The step `code` is the code of, within the drift around `now_s`.
pub(crate) fn step_of(seed: &[u8], code: u32, now_s: i64) -> Option<i64> {
    let now = now_s.div_euclid(STEP_S);
    (now - DRIFT..=now + DRIFT).find(|s| totp(seed, *s) == code)
}

/// The step of the second of two consecutive codes (`EnableMFADevice`), within the last few
/// minutes.
pub(crate) fn consecutive(seed: &[u8], first: u32, second: u32, now_s: i64) -> Option<i64> {
    let now = now_s.div_euclid(STEP_S);
    (now - ENABLE_WINDOW..=now + DRIFT).find(|s| totp(seed, *s) == first && totp(seed, s + 1) == second).map(|s| s + 1)
}

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
    let label = format!("roll:{user}@{account}");
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~@:".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    format!("otpauth://totp/{}?secret={seed}&issuer=roll&algorithm=SHA1&digits=6&period={STEP_S}", enc(&label))
}

/// A QR code of `text` as a PNG (grayscale, 4 pixels per module, a 4-module quiet zone).
pub(crate) fn qr_png(text: &str) -> Option<Vec<u8>> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    const SCALE: usize = 4;
    const QUIET: usize = 4;
    let side = (width + 2 * QUIET) * SCALE;
    let mut raw = Vec::with_capacity(side * (side + 1));
    for y in 0..side {
        raw.push(0u8); // filter: none
        for x in 0..side {
            let (mx, my) = (x / SCALE, y / SCALE);
            let dark = mx >= QUIET && my >= QUIET && mx - QUIET < width && my - QUIET < width && colors[(my - QUIET) * width + (mx - QUIET)] == qrcode::Color::Dark;
            raw.push(if dark { 0 } else { 255 });
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut z, &raw).ok()?;
    let idat = z.finish().ok()?;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(side as u32).to_be_bytes());
    ihdr.extend_from_slice(&(side as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]); // 8-bit grayscale, deflate, no interlace
    for (kind, data) in [(&b"IHDR"[..], &ihdr[..]), (b"IDAT", &idat), (b"IEND", &[])] {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&crc32(&[kind, data].concat()).to_be_bytes());
    }
    Some(png)
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vectors() {
        // RFC 6238 appendix B, SHA-1 (8 digits there; the last 6 are ours).
        let seed = b"12345678901234567890";
        for (t, want) in [(59, 94_287_082u32), (1_111_111_109, 7_081_804), (1_111_111_111, 14_050_471), (1_234_567_890, 89_005_924), (2_000_000_000, 69_279_037), (20_000_000_000, 65_353_130)] {
            assert_eq!(totp(seed, t / STEP_S), want % 1_000_000, "t = {t}");
        }
    }

    #[test]
    fn base32_round_trips() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert_eq!(base32_decode("mzxw6ytboi======").unwrap(), b"foobar");
        assert!(base32_decode("not base32!").is_none());
        let s = new_seed();
        assert_eq!(s.len(), 32);
        assert_eq!(base32_decode(&s).unwrap().len(), 20);
    }

    #[test]
    fn steps_and_drift() {
        let seed = b"12345678901234567890";
        let now = 1_700_000_000;
        let step = now / STEP_S;
        assert_eq!(step_of(seed, totp(seed, step), now), Some(step));
        assert_eq!(step_of(seed, totp(seed, step - 1), now), Some(step - 1));
        assert_eq!(step_of(seed, totp(seed, step - 3), now), None, "too old");
        assert_eq!(consecutive(seed, totp(seed, step - 2), totp(seed, step - 1), now), Some(step - 1));
        assert_eq!(consecutive(seed, totp(seed, step - 2), totp(seed, step), now), None, "not consecutive");
        assert_eq!(parse_code("012345"), Some(12345));
        assert_eq!(parse_code("12345"), None);
        assert_eq!(parse_code("12345a"), None);
    }

    #[test]
    fn throttle_keys() {
        assert_eq!(throttle_key("123456789012", "arn:aws:iam::123456789012:mfa/Team/Alice").as_deref(), Some("mfa:team/alice"));
        assert_eq!(throttle_key("123456789012", "arn:aws:iam::999999999999:mfa/alice"), None);
    }

    #[test]
    fn qr_codes_are_pngs() {
        let png = qr_png(&otpauth_uri("123456789012", "alice", &new_seed())).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(png.ends_with(&[0xAE, 0x42, 0x60, 0x82]), "IEND's CRC");
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }
}
