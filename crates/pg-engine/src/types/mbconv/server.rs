//! Text of a database whose encoding isn't UTF-8.
//!
//! Inside the engine text is a Rust `String` whatever the database's encoding. In a UTF8
//! database that is its server encoding as is. In any other each character of the server
//! encoding is one `char` whose code is an order-preserving number of its bytes:
//! - ASCII is itself;
//! - in SQL_ASCII and the single-byte encodings byte `b` is U+00`b`, which is how
//!   Postgres treats them (a byte is a character: `length`, `ascii`, `chr`, regex
//!   characters, byte order under C collation);
//! - in the multibyte encodings (EUC_*, MULE_INTERNAL) a character's code is 0x80 plus its
//!   rank among the encoding's valid sequences in byte order (skipping the surrogates), so
//!   one character is one `char` and code order is byte order.
//!
//! Bytes become text and text bytes only here: at the client's boundary
//! (`from_client` / `to_client`, pg_any_to_server / pg_server_to_any) and where a function
//! reads or makes the server encoding's bytes (octet_length, convert_to, a text's bytea).

use super::{
    MULE_INTERNAL, SQL_ASCII, UTF8, convert, invalid_encoding, max_length, mblen, utf8_to_local,
    verify,
};
use crate::error::{Error, Result, code};
use crate::types::encnames::encoding_name;
use std::borrow::Cow;

type Class = &'static [(u8, u8)];

const EUC_RANGE: (u8, u8) = (0xa1, 0xfe);
const HIGH: (u8, u8) = (0x80, 0xff);

/// A multibyte encoding's valid characters (wchar.c's verifiers) past ASCII, as classes of
/// byte ranges in byte order. MULE_INTERNAL's LCPRV2 characters are limited to the private
/// charsets Postgres's own conversions make (second byte 0xf0-0xff), so that its classes
/// fit the code space.
fn classes(enc: i32) -> &'static [Class] {
    match enc {
        // EUC_JP / EUC_JIS_2004: SS2 kana, SS3 JIS X 0212, JIS X 0208.
        1 | 5 => &[
            &[(0x8e, 0x8e), (0xa1, 0xdf)],
            &[(0x8f, 0x8f), EUC_RANGE, EUC_RANGE],
            &[EUC_RANGE, EUC_RANGE],
        ],
        // EUC_CN / EUC_KR.
        2 | 3 => &[&[EUC_RANGE, EUC_RANGE]],
        // EUC_TW: CNS 11643 plane 1 (any lead byte), SS2 planes 1-7.
        4 => &[
            &[(0x80, 0x8d), EUC_RANGE],
            &[(0x8e, 0x8e), (0xa1, 0xa7), EUC_RANGE, EUC_RANGE],
            &[(0x90, 0xff), EUC_RANGE],
        ],
        MULE_INTERNAL => &[
            &[(0x80, 0x80)],
            &[(0x81, 0x8d), HIGH],
            &[(0x8e, 0x8f)],
            &[(0x90, 0x99), HIGH, HIGH],
            &[(0x9a, 0x9b), HIGH, HIGH],
            &[(0x9c, 0x9d), (0xf0, 0xff), HIGH, HIGH],
            &[(0x9e, 0xff)],
        ],
        _ => &[],
    }
}

fn class_size(c: Class) -> u32 {
    c.iter().map(|&(a, b)| (b - a) as u32 + 1).product()
}

/// Whether text of `enc` is held as itself (UTF8) rather than as codes.
pub fn is_utf8(enc: i32) -> bool {
    enc == UTF8
}

fn single_byte(enc: i32) -> bool {
    max_length(enc) <= 1
}

fn code_char(rank: u32) -> char {
    let c = 0x80 + rank;
    char::from_u32(if c >= 0xd800 { c + 0x800 } else { c }).unwrap_or('\u{fffd}')
}

fn char_rank(c: char) -> u32 {
    let c = c as u32;
    (if c >= 0xe000 { c - 0x800 } else { c }) - 0x80
}

/// The code of one valid multibyte character `s` of `enc` (`None`: outside the classes).
fn mb_code(enc: i32, s: &[u8]) -> Option<char> {
    let mut base = 0;
    for c in classes(enc) {
        if s.len() == c.len() && (c[0].0..=c[0].1).contains(&s[0]) {
            let mut r = 0u32;
            for (&b, &(lo, hi)) in s.iter().zip(c.iter()) {
                if !(lo..=hi).contains(&b) {
                    return None;
                }
                r = r * ((hi - lo) as u32 + 1) + (b - lo) as u32;
            }
            return Some(code_char(base + r));
        }
        base += class_size(c);
    }
    None
}

/// The bytes of code `c` (past ASCII) of multibyte `enc`, if it is one.
fn mb_bytes(enc: i32, c: char, out: &mut Vec<u8>) -> bool {
    let mut r = char_rank(c);
    for cl in classes(enc) {
        let n = class_size(cl);
        if r < n {
            let at = out.len();
            for &(lo, hi) in cl.iter().rev() {
                let w = (hi - lo) as u32 + 1;
                out.push(lo + (r % w) as u8);
                r /= w;
            }
            out[at..].reverse();
            return true;
        }
        r -= n;
    }
    false
}

/// Server bytes of `enc`, already valid in it, as text.
pub fn decode(enc: i32, b: &[u8]) -> Result<String> {
    if enc == UTF8 {
        return String::from_utf8(b.to_vec()).map_err(|e| invalid_encoding(UTF8, &b[e.utf8_error().valid_up_to()..]));
    }
    if b.is_ascii() {
        return Ok(String::from_utf8(b.to_vec()).expect("ASCII"));
    }
    if single_byte(enc) {
        return Ok(b.iter().map(|&c| c as char).collect());
    }
    let mut out = String::with_capacity(b.len() * 2);
    let mut i = 0;
    while i < b.len() {
        if b[i] < 0x80 {
            out.push(b[i] as char);
            i += 1;
            continue;
        }
        let l = mblen(enc, &b[i..]).min(b.len() - i);
        match mb_code(enc, &b[i..i + l]) {
            Some(c) => out.push(c),
            None => return Err(invalid_encoding(enc, &b[i..])),
        }
        i += l;
    }
    Ok(out)
}

/// Text as the server encoding's bytes. A character that isn't one of `enc`'s codes (a
/// real Unicode character some function made) is converted from Unicode.
pub fn encode(enc: i32, s: &str) -> Result<Cow<'_, [u8]>> {
    if enc == UTF8 || s.is_ascii() {
        return Ok(Cow::Borrowed(s.as_bytes()));
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        if (c as u32) < 0x80 {
            out.push(c as u8);
        } else if single_byte(enc) && (c as u32) <= 0xff {
            out.push(c as u32 as u8);
        } else if !single_byte(enc) && mb_bytes(enc, c, &mut out) {
        } else if enc == SQL_ASCII {
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        } else {
            out.extend_from_slice(&utf8_to_local(c.encode_utf8(&mut buf).as_bytes(), enc)?);
        }
    }
    Ok(Cow::Owned(out))
}

/// The number of server-encoding bytes of `s` (octet_length).
pub fn octet_len(enc: i32, s: &str) -> usize {
    if enc == UTF8 || single_byte(enc) && s.chars().all(|c| (c as u32) <= 0xff) {
        return if enc == UTF8 { s.len() } else { s.chars().count() };
    }
    encode(enc, s).map(|b| b.len()).unwrap_or(s.len())
}

/// Unicode text as text of `enc` (`pg_unicode_to_server` for each character): an error
/// for a character `enc` lacks, or when nothing converts from UTF8 to it (SQL_ASCII,
/// MULE_INTERNAL).
pub fn from_unicode(enc: i32, s: &str) -> Result<String> {
    if enc == UTF8 || s.is_ascii() {
        return Ok(s.to_string());
    }
    if enc == SQL_ASCII || !super::has_conversion(UTF8, enc) {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("conversion between UTF8 and {} is not supported", name(enc)),
        ));
    }
    let b = convert(s.as_bytes(), UTF8, enc)?;
    decode(enc, &b)
}

/// `pg_unicode_to_server_noerror`: `None` where `from_unicode` errors.
pub fn from_unicode_opt(enc: i32, s: &str) -> Option<String> {
    from_unicode(enc, s).ok()
}

/// Text of `enc` as Unicode (for what Postgres hands to ICU or libxml in UTF-8).
pub fn to_unicode(enc: i32, s: &str) -> Result<Cow<'_, str>> {
    if enc == UTF8 || s.is_ascii() {
        return Ok(Cow::Borrowed(s));
    }
    let b = encode(enc, s)?;
    Ok(Cow::Owned(super::to_utf8(&b, enc)?))
}

/// pg_any_to_server: bytes from a client in `client` as text of a database in `server`.
pub fn from_client(b: &[u8], client: i32, server: i32) -> Result<String> {
    if server == UTF8 {
        return super::to_utf8(b, client);
    }
    if client == server || client == SQL_ASCII {
        verify(server, b)?;
    } else if server == SQL_ASCII {
        if crate::types::collation::dblocale::valid_server_encoding(client) {
            verify(client, b)?;
        } else if let Some(&c) = b.iter().find(|&&c| c == 0 || c >= 0x80) {
            return Err(Error::new(
                code::CHARACTER_NOT_IN_REPERTOIRE,
                format!("invalid byte value for encoding \"SQL_ASCII\": 0x{c:02x}"),
            ));
        }
    } else {
        verify(client, b)?;
        let out = convert(b, client, server)?;
        return decode(server, &out);
    }
    decode(server, b)
}

/// pg_server_to_any: text of a database in `server` as bytes for a client in `client`.
pub fn to_client(s: &str, client: i32, server: i32) -> Result<Cow<'_, [u8]>> {
    if client == server && server == UTF8 || s.is_ascii() {
        return Ok(Cow::Borrowed(s.as_bytes()));
    }
    let b = encode(server, s)?;
    Ok(match convert(&b, server, client)? {
        Cow::Borrowed(_) => b,
        Cow::Owned(o) => Cow::Owned(o),
    })
}

/// An encoding's name (for messages).
pub fn name(enc: i32) -> &'static str {
    encoding_name(enc as i64).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_keep_byte_order_and_round_trip() {
        for enc in [1, 2, 3, 4, 5, MULE_INTERNAL] {
            let mut prev: Option<(Vec<u8>, char)> = None;
            let mut total = 0u32;
            for c in classes(enc) {
                total += class_size(c);
            }
            assert!(0x80 + total + 0x800 <= 0x10ffff, "{enc}");
            for r in (0..total).step_by(97).chain([total - 1]) {
                let c = code_char(r);
                let mut b = Vec::new();
                assert!(mb_bytes(enc, c, &mut b));
                assert_eq!(mb_code(enc, &b), Some(c));
                assert_eq!(decode(enc, &b).unwrap(), c.to_string());
                if let Some((pb, pc)) = &prev {
                    assert!(pb < &b && *pc < c);
                }
                prev = Some((b, c));
            }
        }
    }

    #[test]
    fn single_byte_text_is_its_bytes() {
        let b = [b'a', 0xe9, 0x80, 0xff];
        let s = decode(8, &b).unwrap();
        assert_eq!(s.chars().count(), 4);
        assert_eq!(&*encode(8, &s).unwrap(), &b);
        assert_eq!(octet_len(8, &s), 4);
        // é from a UTF8 client into LATIN1, and back.
        let t = from_client("é".as_bytes(), UTF8, 8).unwrap();
        assert_eq!(t, "\u{e9}");
        assert_eq!(&*to_client(&t, UTF8, 8).unwrap(), "é".as_bytes());
        // SQL_ASCII passes bytes through.
        let t = from_client("é".as_bytes(), UTF8, SQL_ASCII).unwrap();
        assert_eq!(t.chars().count(), 2);
        assert_eq!(&*to_client(&t, SQL_ASCII, SQL_ASCII).unwrap(), "é".as_bytes());
    }
}
