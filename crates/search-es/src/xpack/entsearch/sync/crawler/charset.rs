//! A page's character encoding, found as WHATWG's encoding sniffing algorithm finds it (HTML,
//! "Determining the character encoding"), and the page decoded with `encoding_rs` (the WHATWG
//! Encoding Standard's decoders):
//!
//! 1. a byte order mark (UTF-8, UTF-16LE, UTF-16BE);
//! 2. the `Content-Type` header's `charset`;
//! 3. a prescan of the first [`PRESCAN_BYTES`] for `<meta charset>` or
//!    `<meta http-equiv="content-type" content="…charset=…">` (UTF-16 there means UTF-8, and
//!    `x-user-defined` windows-1252, as the algorithm says);
//! 4. the "likely encoding" step: a body that is valid UTF-8 is UTF-8 (unlabeled UTF-8 pages read
//!    as they did before roll sniffed);
//! 5. windows-1252.
//!
//! The prescan reads at most [`PRESCAN_BYTES`] and once; decoding is one linear pass into a
//! buffer no larger than the response's cap.

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252, X_USER_DEFINED};

/// The bytes the prescan looks at.
pub const PRESCAN_BYTES: usize = 1024;

/// The encoding `body` (served with `content_type`) is in, and the length of its byte order
/// mark (0 without one).
pub fn sniff(body: &[u8], content_type: &str) -> (&'static Encoding, usize) {
    if let Some((e, bom)) = Encoding::for_bom(body) {
        return (e, bom);
    }
    if let Some(e) = header_charset(content_type) {
        return (e, 0);
    }
    if let Some(e) = prescan(&body[..body.len().min(PRESCAN_BYTES)]) {
        return (e, 0);
    }
    if std::str::from_utf8(body).is_ok() {
        return (UTF_8, 0);
    }
    (WINDOWS_1252, 0)
}

/// `body` decoded in its encoding ([`sniff`]), into at most `max` bytes of text: past that, the
/// rest is left out (as a page cut at the response cap is). Malformed sequences become U+FFFD.
pub fn decode(body: &[u8], content_type: &str, max: usize) -> (String, &'static Encoding) {
    let (encoding, bom) = sniff(body, content_type);
    let src = &body[bom..];
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let room = decoder.max_utf8_buffer_length(src.len()).unwrap_or(max).min(max);
    let mut out = String::with_capacity(room);
    // Writes into `out`'s spare capacity only: never past `room`.
    let _ = decoder.decode_to_string(src, &mut out, true);
    (out, encoding)
}

/// The `charset` parameter of a `Content-Type` value, when it names an encoding.
fn header_charset(content_type: &str) -> Option<&'static Encoding> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k.trim().eq_ignore_ascii_case("charset")).then(|| v.trim().trim_matches(|c| c == '"' || c == '\'')).and_then(|v| Encoding::for_label(v.as_bytes()))
    })
}

/// What a `<meta>` names as the encoding, adjusted as the prescan adjusts it.
fn from_meta(label: &[u8]) -> Option<&'static Encoding> {
    let e = Encoding::for_label(label)?;
    Some(match e {
        e if e == UTF_16BE || e == UTF_16LE => UTF_8,
        e if e == X_USER_DEFINED => WINDOWS_1252,
        e => e,
    })
}

fn is_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// The prescan ("prescan a byte stream to determine its encoding") over `b`: comments skipped,
/// other tags' attributes read past, and the first `<meta>` naming an encoding decides.
fn prescan(b: &[u8]) -> Option<&'static Encoding> {
    let starts = |at: usize, s: &[u8]| b.len() >= at + s.len() && b[at..at + s.len()].eq_ignore_ascii_case(s);
    let mut i = 0;
    while i < b.len() {
        if starts(i, b"<!--") {
            // To the end of the comment (its `-->` may share the opening's dashes).
            i = b[i + 2..].windows(3).position(|w| w == b"-->").map_or(b.len(), |p| i + 2 + p + 3);
            continue;
        }
        if starts(i, b"<meta") && b.get(i + 5).is_some_and(|c| is_space(*c) || *c == b'/') {
            i += 5;
            let (mut got_pragma, mut need_pragma, mut charset): (bool, Option<bool>, Option<&'static Encoding>) = (false, None, None);
            let mut seen: Vec<Vec<u8>> = Vec::new();
            while let Some((name, value, next)) = attribute(b, i) {
                i = next;
                if seen.contains(&name) {
                    continue;
                }
                match name.as_slice() {
                    b"http-equiv" if value.eq_ignore_ascii_case(b"content-type") => got_pragma = true,
                    b"content" if charset.is_none() => {
                        if let Some(e) = content_charset(&value).as_deref().and_then(from_meta) {
                            charset = Some(e);
                            need_pragma = Some(true);
                        }
                    }
                    b"charset" => {
                        charset = from_meta(&value);
                        need_pragma = Some(false);
                    }
                    _ => {}
                }
                seen.push(name);
            }
            match (need_pragma, charset) {
                (Some(true), Some(_)) if !got_pragma => {}
                (Some(_), Some(e)) => return Some(e),
                _ => {}
            }
            continue;
        }
        if b[i] == b'<' && (b.get(i + 1).is_some_and(u8::is_ascii_alphabetic) || (b.get(i + 1) == Some(&b'/') && b.get(i + 2).is_some_and(u8::is_ascii_alphabetic))) {
            // A tag: past its name, then its attributes.
            i += 1;
            while i < b.len() && !is_space(b[i]) && b[i] != b'>' {
                i += 1;
            }
            while let Some((_, _, next)) = attribute(b, i) {
                i = next;
            }
            i += 1;
            continue;
        }
        if starts(i, b"<!") || starts(i, b"</") || starts(i, b"<?") {
            i = b[i..].iter().position(|c| *c == b'>').map_or(b.len(), |p| i + p + 1);
            continue;
        }
        i += 1;
    }
    None
}

/// The prescan's "get an attribute" at `i`: its lowercased name, its value and where reading
/// goes on; `None` at the tag's end (or the bytes').
fn attribute(b: &[u8], mut i: usize) -> Option<(Vec<u8>, Vec<u8>, usize)> {
    while i < b.len() && (is_space(b[i]) || b[i] == b'/') {
        i += 1;
    }
    if i >= b.len() || b[i] == b'>' {
        return None;
    }
    let mut name = Vec::new();
    while i < b.len() {
        let c = b[i];
        if (c == b'=' && !name.is_empty()) || is_space(c) || c == b'/' || c == b'>' {
            break;
        }
        name.push(c.to_ascii_lowercase());
        i += 1;
    }
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    if i >= b.len() || b[i] != b'=' {
        return Some((name, Vec::new(), i));
    }
    i += 1;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    let mut value = Vec::new();
    match b.get(i) {
        Some(&q) if q == b'"' || q == b'\'' => {
            i += 1;
            while i < b.len() && b[i] != q {
                value.push(b[i].to_ascii_lowercase());
                i += 1;
            }
            i += 1;
        }
        _ => {
            while i < b.len() && !is_space(b[i]) && b[i] != b'>' {
                value.push(b[i].to_ascii_lowercase());
                i += 1;
            }
        }
    }
    Some((name, value, i.min(b.len())))
}

/// The encoding label a `content` attribute names ("extracting a character encoding from a meta
/// element").
fn content_charset(v: &[u8]) -> Option<Vec<u8>> {
    let mut i = 0;
    loop {
        let at = v[i..].windows(7).position(|w| w.eq_ignore_ascii_case(b"charset"))? + i + 7;
        let mut j = at;
        while j < v.len() && is_space(v[j]) {
            j += 1;
        }
        if v.get(j) != Some(&b'=') {
            i = at;
            continue;
        }
        j += 1;
        while j < v.len() && is_space(v[j]) {
            j += 1;
        }
        return match v.get(j) {
            Some(&q) if q == b'"' || q == b'\'' => {
                let rest = &v[j + 1..];
                rest.iter().position(|c| *c == q).map(|end| rest[..end].to_vec())
            }
            Some(_) => Some(v[j..].iter().take_while(|c| !is_space(**c) && **c != b';').copied().collect()),
            None => None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing_follows_the_whatwg_order() {
        let name = |body: &[u8], ct: &str| sniff(body, ct).0.name();
        assert_eq!(sniff(b"\xEF\xBB\xBF<p>x", "text/html; charset=iso-8859-2"), (UTF_8, 3), "a BOM first");
        assert_eq!(name(b"\xFF\xFEa\0", "text/html"), "UTF-16LE");
        assert_eq!(name(b"<meta charset=shift_jis>", "text/html; charset=\"ISO-8859-2\""), "ISO-8859-2", "then the header");
        assert_eq!(name(b"<!-- <meta charset=koi8-r> --><META Charset='Shift_JIS'>", "text/html"), "Shift_JIS", "a comment is skipped");
        assert_eq!(name(br#"<meta http-equiv="Content-Type" content="text/html; charset=euc-jp">"#, "text/html"), "EUC-JP");
        assert_eq!(name(b"<meta content=\"text/html; charset=euc-jp\">caf\xE9", "text/html"), "windows-1252", "content without http-equiv doesn't count");
        assert_eq!(name(b"<div title=\"<meta charset=koi8-r>\"></div>caf\xE9", "text/html"), "windows-1252", "inside another tag's attribute");
        assert_eq!(name(b"<meta charset=utf-16le>", "text/html"), "UTF-8");
        assert_eq!(name(b"<p>caf\xC3\xA9</p>", "text/html"), "UTF-8", "valid UTF-8");
        assert_eq!(name(b"<p>caf\xE9</p>", "text/html"), "windows-1252", "the fallback");
        let late = [b" ".repeat(PRESCAN_BYTES), b"<meta charset=shift_jis>".to_vec()].concat();
        assert_eq!(name(&late, "text/html"), "UTF-8", "past the prescan's bytes");
    }

    #[test]
    fn decoding_stays_within_its_cap() {
        assert_eq!(decode(b"caf\xE9 \x93q\x94", "text/html", 1 << 20).0, "café “q”");
        assert_eq!(decode(b"\x93\xfa\x96\x7b", "text/html; charset=shift_jis", 1 << 20).0, "日本");
        let (text, _) = decode(&[0xE9; 1000], "text/html; charset=windows-1252", 100);
        assert!(text.len() <= 100 && text.chars().all(|c| c == 'é'), "cut at the cap: {}", text.len());
    }
}
