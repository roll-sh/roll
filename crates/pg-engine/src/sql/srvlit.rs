//! Escapes in string literals of a database whose encoding isn't UTF8.
//!
//! libpg_query's scanner works in UTF-8: an `E'\xe9'` byte it rejects as invalid UTF-8,
//! and a `é` / `U&'\00e9'` escape it makes into UTF-8. Postgres's scanner makes both
//! into the database encoding's bytes (scan.l `addlitchar`, `addunicode` →
//! `pg_unicode_to_server`) and checks the result in it (`pg_verifymbstr`). So before
//! parsing, such literals of a non-UTF8 database are rewritten as standard ones holding
//! their value (mbconv::server's codes), padded after the closing quote to keep the
//! character positions of everything else (as scs.rs does for ordinary literals).

use crate::error::{Error, Result};
use crate::types::mbconv::{self, server};

/// `sql` with its escaped literals' values made standard literals, for a database in
/// `enc`; `None` when nothing changes. `standard_strings`: whether ordinary `'...'`
/// literals take no escapes.
pub fn rewrite(sql: &str, enc: i32, standard_strings: bool) -> Result<Option<(String, Vec<Error>)>> {
    if enc == mbconv::UTF8 || !sql.contains('\\') {
        return Ok(None);
    }
    let b = sql.as_bytes();
    let n = b.len();
    let char_pos = |at: usize| sql[..at].chars().count() as u32 + 1;
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    // The scanner's warnings for the ordinary literals rewritten (with
    // standard_conforming_strings off), as scs.rs gives them.
    let mut warnings = Vec::new();
    let mut i = 0;
    while i < n {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == b'-' && next == Some(b'-') {
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && next == Some(b'*') {
            let mut depth = 0;
            while i < n {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if c == b'"' {
            i = skip(b, i + 1, b'"');
            continue;
        }
        if c == b'$' && (i == 0 || !is_ident(b[i - 1])) {
            let mut j = i + 1;
            if j < n && (b[j].is_ascii_alphabetic() || b[j] == b'_' || b[j] >= 0x80) {
                while j < n && is_ident(b[j]) {
                    j += 1;
                }
            }
            if b.get(j) == Some(&b'$') {
                let tag = &sql[i..=j];
                i = match sql[j + 1..].find(tag) {
                    Some(k) => j + 1 + k + tag.len(),
                    None => n,
                };
                continue;
            }
            i += 1;
            continue;
        }
        let prev_ident = i > 0 && is_ident(b[i - 1]);
        // E'..' (or, standard_conforming_strings off, '..'): backslash escapes.
        let e_start = matches!(c, b'e' | b'E') && next == Some(b'\'') && !prev_ident;
        if e_start || c == b'\'' && !standard_strings {
            let q = if e_start { i + 1 } else { i };
            let (end, v) = escaped(sql, q, enc)?;
            if let Some(v) = v {
                if !e_start {
                    let mut o = super::scs::opts();
                    o.standard_strings = false;
                    let pad = " ".repeat(char_pos(i) as usize - 1);
                    warnings.extend(super::scs::scan(&format!("{pad}{}", &sql[i..end]), o, None).warnings);
                }
                edits.push((i, end, literal(&v, sql[i..end].chars().count())));
            }
            i = end;
            continue;
        }
        // U&'..' [UESCAPE 'c'].
        if matches!(c, b'u' | b'U') && next == Some(b'&') && b.get(i + 2) == Some(&b'\'') && !prev_ident {
            let close = skip(b, i + 3, b'\'');
            let (esc, end) = uescape(sql, close);
            let body = sql[i + 3..close.saturating_sub(1).max(i + 3)].replace("''", "'");
            match unicode_body(&body, esc, enc) {
                Ok(Some(v)) => edits.push((i, end, literal(&v, sql[i..end].chars().count()))),
                Ok(None) => {}
                Err(b) => {
                    let (e, off) = *b;
                    // (Positioned at the escape: str_udeescape's scanner_errposition.)
                    let at = i + 3 + body_offset(&sql[i + 3..], off);
                    return Err(e.position(char_pos(at)));
                }
            }
            i = end;
            continue;
        }
        if c == b'\'' {
            i = skip(b, i + 1, b'\'');
            continue;
        }
        if matches!(c, b'b' | b'B' | b'x' | b'X' | b'n' | b'N') && next == Some(b'\'') && !prev_ident {
            i = skip(b, i + 2, b'\'');
            continue;
        }
        i += 1;
    }
    if edits.is_empty() {
        return Ok(None);
    }
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    for (s, e, lit) in edits {
        out.push_str(&sql[at..s]);
        out.push_str(&lit);
        at = e;
    }
    out.push_str(&sql[at..]);
    Ok(Some((out, warnings)))
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// The index after the quoted text whose opening quote `q` precedes `from` (doubled quotes
/// inside; `n` when unterminated).
fn skip(b: &[u8], mut i: usize, q: u8) -> usize {
    while i < b.len() {
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// The byte offset in `s` of its unescaped body's `off`th byte (doubled quotes counted once).
fn body_offset(s: &str, off: usize) -> usize {
    let b = s.as_bytes();
    let (mut i, mut k) = (0, 0);
    while k < off && i < b.len() {
        i += if b[i] == b'\'' && b.get(i + 1) == Some(&b'\'') { 2 } else { 1 };
        k += 1;
    }
    i
}

/// A standard literal holding `v`, padded to `width` characters.
fn literal(v: &str, width: usize) -> String {
    // (A backslash stays one under any standard_conforming_strings: E'\\'.)
    let mut lit = String::with_capacity(v.len() + 3);
    if v.contains('\\') {
        lit.push('E');
    }
    lit.push('\'');
    lit.push_str(&v.replace('\'', "''").replace('\\', "\\\\"));
    lit.push('\'');
    let pad = width.saturating_sub(lit.chars().count());
    lit.extend(std::iter::repeat_n(' ', pad));
    lit
}

/// An escaped literal whose opening quote is at `q`: where it ends, and its value when an
/// escape made a byte or character the UTF-8 scanner would read differently. Malformed
/// escapes are left to the scanner.
fn escaped(sql: &str, q: usize, enc: i32) -> Result<(usize, Option<String>)> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut v: Vec<u8> = Vec::new();
    let mut changed = false;
    let mut j = q + 1;
    let mut buf = [0u8; 4];
    loop {
        if j >= n {
            return Ok((n, None));
        }
        match b[j] {
            b'\'' if b.get(j + 1) == Some(&b'\'') => {
                v.push(b'\'');
                j += 2;
            }
            b'\'' => {
                j += 1;
                // A continuation (whitespace with a newline, then a quote) goes on.
                let mut k = j;
                let mut nl = false;
                while k < n && b[k].is_ascii_whitespace() {
                    nl |= b[k] == b'\n';
                    k += 1;
                }
                if nl && b.get(k) == Some(&b'\'') {
                    return Ok((j, None));
                }
                break;
            }
            b'\\' => {
                let Some(&e) = b.get(j + 1) else {
                    return Ok((n, None));
                };
                match e {
                    b'0'..=b'7' => {
                        let mut k = j + 1;
                        let mut val: u32 = 0;
                        while k < n && k < j + 4 && (b'0'..=b'7').contains(&b[k]) {
                            val = val * 8 + (b[k] - b'0') as u32;
                            k += 1;
                        }
                        changed |= val as u8 >= 0x80;
                        v.push(val as u8);
                        j = k;
                    }
                    b'x' if b.get(j + 2).is_some_and(u8::is_ascii_hexdigit) => {
                        let mut k = j + 2;
                        let mut val: u32 = 0;
                        while k < n && k < j + 4 && b[k].is_ascii_hexdigit() {
                            val = val * 16 + (b[k] as char).to_digit(16).unwrap_or(0);
                            k += 1;
                        }
                        changed |= val >= 0x80;
                        v.push(val as u8);
                        j = k;
                    }
                    b'u' | b'U' => {
                        let len = if e == b'u' { 4 } else { 8 };
                        let Some(cp) = sql
                            .get(j + 2..j + 2 + len)
                            .filter(|h| h.bytes().all(|d| d.is_ascii_hexdigit()))
                            .and_then(|h| u32::from_str_radix(h, 16).ok())
                        else {
                            return Ok((n, None));
                        };
                        // (Surrogate pairs and invalid code points: the scanner's errors.)
                        let Some(ch) = char::from_u32(cp) else {
                            return Ok((n, None));
                        };
                        if cp >= 0x80 {
                            changed = true;
                            let at = sql[..j].chars().count() as u32 + 1;
                            let s = server::from_unicode(enc, ch.encode_utf8(&mut buf))
                                .map_err(|e| e.position(at))?;
                            v.extend_from_slice(&server::encode(enc, &s)?);
                        } else {
                            v.push(cp as u8);
                        }
                        j += 2 + len;
                    }
                    b'b' | b'f' | b'n' | b'r' | b't' => {
                        v.push(match e {
                            b'b' => 8,
                            b'f' => 12,
                            b'n' => b'\n',
                            b'r' => b'\r',
                            _ => b'\t',
                        });
                        j += 2;
                    }
                    _ => {
                        let l = sql[j + 1..].chars().next().map_or(1, char::len_utf8);
                        let ch = &sql[j + 1..j + 1 + l];
                        v.extend_from_slice(&server::encode(enc, ch)?);
                        j += 1 + l;
                    }
                }
            }
            _ => {
                let l = sql[j..].chars().next().map_or(1, char::len_utf8);
                v.extend_from_slice(&server::encode(enc, &sql[j..j + l])?);
                j += l;
            }
        }
    }
    if !changed {
        return Ok((j, None));
    }
    // (A NUL is the scanner's error: left to it.)
    if v.contains(&0) {
        return Ok((j, None));
    }
    mbconv::verify(enc, &v)?;
    Ok((j, Some(server::decode(enc, &v)?)))
}

/// After a U&'..' literal ending at `close`: its UESCAPE character and where the clause ends.
fn uescape(sql: &str, close: usize) -> (char, usize) {
    let rest = &sql[close..];
    let t = rest.trim_start();
    let ws = rest.len() - t.len();
    if t.len() >= 7 && t[..7].eq_ignore_ascii_case("uescape") {
        let u = t[7..].trim_start();
        let ws2 = t.len() - 7 - u.len();
        let ub = u.as_bytes();
        if ub.len() >= 3 && ub[0] == b'\'' && ub[2] == b'\'' {
            return (ub[1] as char, close + ws + 7 + ws2 + 3);
        }
    }
    ('\\', close)
}

/// A U&'..' body's value when it has escapes past ASCII (`Err`: an error and the body
/// offset of its escape); malformed escapes are left to the parser.
fn unicode_body(body: &str, esc: char, enc: i32) -> std::result::Result<Option<String>, Box<(Error, usize)>> {
    let mut out = String::with_capacity(body.len());
    let mut changed = false;
    let mut first: Option<(u32, usize)> = None;
    let mut it = body.char_indices().peekable();
    while let Some((at, c)) = it.next() {
        if c != esc {
            if first.is_some() {
                return Ok(None);
            }
            out.push(c);
            continue;
        }
        if it.peek().map(|p| p.1) == Some(esc) {
            it.next();
            out.push(esc);
            continue;
        }
        let rest = &body[at + c.len_utf8()..];
        let (digits, len) = match rest.strip_prefix('+') {
            Some(r) => (r.get(..6), 7),
            None => (rest.get(..4), 4),
        };
        let Some(cp) = digits
            .filter(|h| h.bytes().all(|d| d.is_ascii_hexdigit()))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
        else {
            return Ok(None);
        };
        for _ in 0..len {
            it.next();
        }
        let cp = match first.take() {
            Some((hi, _)) if (0xdc00..0xe000).contains(&cp) => 0x10000 + ((hi - 0xd800) << 10) + (cp - 0xdc00),
            Some(_) => return Ok(None),
            None if (0xd800..0xdc00).contains(&cp) => {
                first = Some((cp, at));
                continue;
            }
            None => cp,
        };
        let Some(ch) = char::from_u32(cp).filter(|_| cp != 0) else {
            return Ok(None);
        };
        if cp >= 0x80 {
            changed = true;
            let s = server::from_unicode(enc, ch.encode_utf8(&mut [0; 4])).map_err(|e| Box::new((e, at)))?;
            out.push_str(&s);
        } else {
            out.push(ch);
        }
    }
    Ok((changed && first.is_none()).then_some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_become_server_characters() {
        let sql = "SELECT E'\\xe9', U&'\\00e9x' -- E'\\xff'";
        let r = rewrite(sql, 8, true).unwrap().unwrap().0;
        // (Character positions kept: the literals padded.)
        assert_eq!(r.chars().count(), sql.chars().count());
        assert_eq!(r.split_whitespace().collect::<Vec<_>>(), ["SELECT", "'\u{e9}'", ",", "'\u{e9}x'", "--", "E'\\xff'"]);
        assert!(rewrite("SELECT E'\\n'", 8, true).unwrap().is_none());
        assert!(rewrite("SELECT E'\\xe9'", mbconv::UTF8, true).unwrap().is_none());
        // € isn't LATIN1's: pg_unicode_to_server's error, at the escape.
        let e = rewrite("SELECT E'\\u20ac'", 8, true).unwrap_err();
        assert_eq!(e.position, Some(10));
    }
}
