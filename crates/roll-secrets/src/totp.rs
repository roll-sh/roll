//! TOTP keys (RFC 6238; docs/secrets.md, "TOTP keys"): a key whose value is a TOTP seed hands out
//! the current one-time code, as Vault's TOTP engine and 1Password's one-time passwords do.
//!
//! A seed is an `otpauth://totp/…` URI (`secret`, `algorithm`, `digits`, `period`) or bare base32
//! (SHA1, 6 digits, 30 s). A key is one when it's typed `totp` (a required key's type) or its value
//! is an `otpauth://totp/` URI. Errors name the rule, never the seed; a code is a credential, so it
//! goes in no audit row, log or `Debug`.

use crate::access::{Action, Scope};
use crate::required::KeyType;
use crate::{Caller, Error, Result, Secrets, Value, Values, names};
use serde::Serialize;
use zeroize::Zeroizing;

/// The fewest and most bytes a seed may have (80 bits is what most services hand out).
const MIN_SECRET: usize = 10;
const MAX_SECRET: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

/// A parsed seed. Redacted in `Debug`, zeroized on drop.
pub struct Totp {
    secret: Zeroizing<Vec<u8>>,
    pub algorithm: Algorithm,
    pub digits: u32,
    pub period: u64,
}

impl std::fmt::Debug for Totp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Totp(<redacted>)")
    }
}

/// The current code of a TOTP key, and when it stops being valid (ms since the epoch).
#[derive(Serialize)]
pub struct Code {
    pub code: Value,
    pub expires_at: i64,
    /// Seconds each code is valid for.
    pub period: u64,
}

impl std::fmt::Debug for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Code").field("code", &self.code).field("expires_at", &self.expires_at).finish()
    }
}

/// RFC 4648 base32, any case, spaces and padding ignored (1Password shows seeds in groups of four).
fn base32(s: &str) -> Option<Zeroizing<Vec<u8>>> {
    let mut out = Zeroizing::new(Vec::with_capacity(s.len() * 5 / 8));
    let (mut buf, mut bits) = (0u64, 0u32);
    for c in s.trim_end_matches('=').bytes().filter(|c| *c != b' ') {
        let v = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        buf = (buf << 5) | u64::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// `%XX` decoded (a URI's query value); `None` on a broken escape.
fn percent_decode(s: &str) -> Option<Zeroizing<String>> {
    let b = s.as_bytes();
    let mut out = Zeroizing::new(Vec::with_capacity(b.len()));
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8(std::mem::take(&mut *out)).ok().map(Zeroizing::new)
}

/// Whether `v` claims to be a TOTP URI (so an untyped key with one is a TOTP key).
pub fn is_uri(v: &str) -> bool {
    v.get(..15).is_some_and(|p| p.eq_ignore_ascii_case("otpauth://totp/"))
}

/// Parse a seed. The error says what's wrong, never what the value is.
pub fn parse(v: &str) -> std::result::Result<Totp, &'static str> {
    let v = v.trim();
    let mut t = Totp { secret: Zeroizing::new(vec![]), algorithm: Algorithm::Sha1, digits: 6, period: 30 };
    if v.get(..10).is_some_and(|p| p.eq_ignore_ascii_case("otpauth://")) {
        if !is_uri(v) {
            return Err("an otpauth URI must be otpauth://totp/… (HOTP isn't supported)");
        }
        let query = v.split_once('?').map(|(_, q)| q.split('#').next().unwrap_or(q)).ok_or("an otpauth URI needs ?secret=")?;
        let mut secret = None;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, val) = pair.split_once('=').unwrap_or((pair, ""));
            let val = percent_decode(val).ok_or("an otpauth URI's query is badly escaped")?;
            match k.to_ascii_lowercase().as_str() {
                "secret" => secret = Some(base32(&val).ok_or("secret must be base32")?),
                "algorithm" => {
                    t.algorithm = match val.to_ascii_uppercase().as_str() {
                        "SHA1" => Algorithm::Sha1,
                        "SHA256" => Algorithm::Sha256,
                        "SHA512" => Algorithm::Sha512,
                        _ => return Err("algorithm must be SHA1, SHA256 or SHA512"),
                    }
                }
                "digits" => t.digits = val.parse().ok().filter(|d| (6..=8).contains(d)).ok_or("digits must be 6–8")?,
                "period" => t.period = val.parse().ok().filter(|p| (15..=120).contains(p)).ok_or("period must be 15–120 seconds")?,
                _ => {}
            }
        }
        t.secret = secret.ok_or("an otpauth URI needs ?secret=")?;
    } else {
        t.secret = base32(v).ok_or("a TOTP seed is an otpauth://totp/ URI or base32")?;
    }
    if t.secret.len() < MIN_SECRET || t.secret.len() > MAX_SECRET {
        return Err("a TOTP secret is 16–205 base32 characters (80–1024 bits)");
    }
    Ok(t)
}

impl Totp {
    /// The code for the step holding `unix` seconds (RFC 6238, T0 = 0).
    pub fn code_at(&self, unix: u64) -> Zeroizing<String> {
        let alg = match self.algorithm {
            Algorithm::Sha1 => ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
            Algorithm::Sha256 => ring::hmac::HMAC_SHA256,
            Algorithm::Sha512 => ring::hmac::HMAC_SHA512,
        };
        let key = ring::hmac::Key::new(alg, &self.secret);
        let tag = ring::hmac::sign(&key, &(unix / self.period).to_be_bytes());
        let h = tag.as_ref();
        let o = usize::from(h[h.len() - 1] & 0x0f);
        let n = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]) % 10u32.pow(self.digits);
        Zeroizing::new(format!("{n:0width$}", width = self.digits as usize))
    }

    /// The code at `now_ms`, and when it expires.
    pub fn code(&self, now_ms: i64) -> Code {
        let unix = u64::try_from(now_ms / 1000).unwrap_or(0);
        let expires = (unix / self.period + 1) * self.period;
        Code { code: Value::new(self.code_at(unix).as_str()), expires_at: i64::try_from(expires).unwrap_or(i64::MAX).saturating_mul(1000), period: self.period }
    }
}

impl Secrets {
    /// Whether `key` in folder `path` is typed `totp`.
    fn typed_totp(&self, project: &str, path: &str, key: &str) -> Result<bool> {
        Ok(self.db.read(|c| crate::required::of(c, project))?.iter().any(|r| r.path == path && r.key == key && r.kind == Some(KeyType::Totp)))
    }

    /// The current code of a TOTP key (`GET …/secrets/{key}/totp`). Needs what a reveal needs (the
    /// code is a credential), and is audited as `secrets.totp` without the code.
    pub fn totp(&self, project: &str, slug: &str, path: &str, key: &str, caller: &Caller) -> Result<Code> {
        let path = names::path(path)?;
        caller.need(Action::Reveal, &Scope::at(project, slug, &path))?;
        let narrowed = self.caller_for(project, slug, caller)?;
        let (seed, problems, leases) = self.reveal_entry(project, slug, &path, key, true, &narrowed)?;
        let t = (|| {
            if let Some(p) = problems.first() {
                return Err(Error::Conflict("unresolved", format!("{key} didn't resolve: {}", p.problem)));
            }
            if !self.typed_totp(project, &path, key)? && !is_uri(seed.expose()) {
                return Err(Error::BadRequest(format!("{key} isn't a TOTP key (type it totp, or store an otpauth://totp/ URI)")));
            }
            parse(seed.expose()).map_err(|why| Error::BadRequest(format!("{key}: {why}")))
        })();
        let t = match t {
            Ok(t) => t,
            Err(e) => {
                // No code, so nothing the read issued (a dynamic link's lease) may outlive it.
                for lease in &leases {
                    self.revoke_leases_where("id = ?", lease);
                }
                return Err(e);
            }
        };
        let code = t.code(self.db.now_ms()?);
        self.audit_now(project, &narrowed, "secrets.totp", Some(slug), Some(&path), &[key.to_string()], &format!("made a one-time code of {key}{}", crate::read::lease_note(&leases)));
        Ok(code)
    }

    /// Add `<KEY>_CODE` beside each TOTP key of a read (`codes: true` on a resolve, `roll run
    /// --totp-codes`): the code when the values were read, valid for at most one period. A name
    /// the folder already uses is left alone; a code of a sensitive key is sensitive too.
    pub(crate) fn add_codes(&self, project: &str, path: &str, v: &mut Values) -> Result<()> {
        let path = names::path(path)?;
        let typed: Vec<String> = self.db.read(|c| crate::required::of(c, project))?.into_iter().filter(|r| r.path == path && r.kind == Some(KeyType::Totp)).map(|r| r.key).collect();
        let now = self.db.now_ms()?;
        let mut codes = vec![];
        for (k, val) in &v.values {
            if !(typed.contains(k) || is_uri(val.expose())) {
                continue;
            }
            let name = format!("{k}_CODE");
            if v.values.contains_key(&name) {
                continue;
            }
            if let Ok(t) = parse(val.expose()) {
                codes.push((k.clone(), name, t.code(now).code));
            }
        }
        for (k, name, code) in codes {
            if v.sensitive.contains(&k) {
                v.sensitive.push(name.clone());
            }
            v.values.insert(name, code);
        }
        v.sensitive.sort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238, Appendix B: each algorithm's seed, 8 digits, 30 s steps.
    #[test]
    fn rfc_6238_vectors() {
        let seeds = [
            ("SHA1", "12345678901234567890".to_string()),
            ("SHA256", "12345678901234567890123456789012".to_string()),
            ("SHA512", "1234567890".repeat(6) + "1234"),
        ];
        let vectors: [(u64, [&str; 3]); 6] = [
            (59, ["94287082", "46119246", "90693936"]),
            (1111111109, ["07081804", "68084774", "25091201"]),
            (1111111111, ["14050471", "67062674", "99943326"]),
            (1234567890, ["89005924", "91819424", "93441116"]),
            (2000000000, ["69279037", "90698825", "38618901"]),
            (20000000000, ["65353130", "77737706", "47863826"]),
        ];
        for (i, (alg, seed)) in seeds.iter().enumerate() {
            let uri = format!("otpauth://totp/roll:test?secret={}&algorithm={alg}&digits=8&period=30", encode(seed.as_bytes()));
            let t = parse(&uri).unwrap();
            for (time, codes) in &vectors {
                assert_eq!(t.code_at(*time).as_str(), codes[i], "{alg} at {time}");
            }
        }
        // Bare base32 is SHA1, 6 digits, 30 s: the vector's last six digits.
        let t = parse("gezd gnbv gy3t qojq gezd gnbv gy3t qojq").unwrap();
        assert_eq!(t.code_at(59).as_str(), "287082");
        let c = t.code(59_500);
        assert_eq!((c.code.expose(), c.expires_at, c.period), ("287082", 60_000, 30));
    }

    #[test]
    fn malformed_seeds_are_refused_without_quoting_them() {
        for bad in ["", "not base32!", "GEZDGNBV", "otpauth://hotp/x?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", "otpauth://totp/x", "otpauth://totp/x?secret=GEZDGNBVGY3TQOJQ&digits=9", "otpauth://totp/x?secret=GEZDGNBVGY3TQOJQ&period=5", "otpauth://totp/x?secret=GEZDGNBVGY3TQOJQ&algorithm=MD5"] {
            let e = parse(bad).unwrap_err();
            assert!(bad.len() < 8 || !e.contains(bad), "{e}");
        }
        assert!(format!("{:?}", parse("GEZDGNBVGY3TQOJQ").unwrap()).contains("redacted"));
    }

    fn encode(b: &[u8]) -> String {
        const A: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let (mut out, mut buf, mut bits) = (String::new(), 0u32, 0);
        for &x in b {
            buf = (buf << 8) | u32::from(x);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(A[((buf >> bits) & 31) as usize] as char);
            }
            buf &= (1 << bits) - 1;
        }
        if bits > 0 {
            out.push(A[((buf << (5 - bits)) & 31) as usize] as char);
        }
        out
    }
}
