//! Byte-exact text in `String`s. Status and error replies are `String`s, but Redis echoes the
//! caller's bytes in them (unknown command names, options, keys…), which needn't be UTF-8.
//! [`escape`] keeps any byte string losslessly: bytes that aren't UTF-8, and the characters of the
//! reserved block U+10FF00..=U+10FFFF themselves, are written byte by byte as U+10FF00 + byte;
//! [`unescape`] (applied when a reply is encoded) gives the bytes back. Text without such bytes
//! is unchanged.

use std::borrow::Cow;

const BASE: u32 = 0x10FF00;

fn push_byte(out: &mut String, b: u8) {
    out.push(char::from_u32(BASE + b as u32).expect("in range"));
}

fn push_valid(out: &mut String, s: &str) {
    for c in s.chars() {
        if c as u32 >= BASE {
            let mut buf = [0u8; 4];
            for &b in c.encode_utf8(&mut buf).as_bytes() {
                push_byte(out, b);
            }
        } else {
            out.push(c);
        }
    }
}

/// `bytes` as a `String` [`unescape`] turns back into exactly `bytes`.
pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                push_valid(&mut out, s);
                return out;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                push_valid(&mut out, std::str::from_utf8(valid).expect("validated"));
                let bad = e.error_len().unwrap_or(after.len());
                for &b in &after[..bad] {
                    push_byte(&mut out, b);
                }
                rest = &after[bad..];
            }
        }
    }
}

/// The bytes an [`escape`]d text stands for.
pub fn unescape(s: &str) -> Cow<'_, [u8]> {
    if !s.chars().any(|c| c as u32 >= BASE) {
        return Cow::Borrowed(s.as_bytes());
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        let u = c as u32;
        if u >= BASE {
            out.push((u - BASE) as u8);
        } else {
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    Cow::Owned(out)
}

/// The first `n` bytes of `bytes` (Redis' `%.*s`), [`escape`]d.
pub fn escape_prefix(bytes: &[u8], n: usize) -> String {
    escape(&bytes[..bytes.len().min(n)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_any_bytes() {
        let cases: [&[u8]; 7] = [b"plain", b"\xff\xfe", "h\u{e9}llo".as_bytes(), b"a\xc3", "\u{10FF41}x".as_bytes(), "\u{10FFFF}".as_bytes(), b""];
        for c in cases {
            assert_eq!(&*unescape(&escape(c)), c, "{c:?}");
        }
        // Valid text is kept as is.
        assert_eq!(escape("héllo".as_bytes()), "héllo");
        let mut x = 7u64;
        for _ in 0..2000 {
            let len = (x % 9) as usize;
            let v: Vec<u8> = (0..len)
                .map(|_| {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (x >> 33) as u8
                })
                .collect();
            assert_eq!(&*unescape(&escape(&v)), &v[..]);
        }
    }
}
