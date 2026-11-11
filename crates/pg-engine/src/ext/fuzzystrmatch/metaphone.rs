//! `metaphone(text, int)` (fuzzystrmatch.c's `_metaphone`, non-traditional variant).

use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::{FnCtx, text_arg};
use crate::types::Value;

const MAX_METAPHONE_STRLEN: usize = 255;
const SH: u8 = b'X';
const TH: u8 = b'0';
const CODES: [u8; 26] = [1, 16, 4, 16, 9, 2, 4, 16, 9, 2, 0, 2, 2, 2, 1, 4, 0, 2, 4, 4, 1, 0, 0, 0, 8, 0];

fn getcode(c: u8) -> u8 {
    if c.is_ascii_alphabetic() { CODES[(c.to_ascii_uppercase() - b'A') as usize] } else { 0 }
}
fn isvowel(c: u8) -> bool {
    getcode(c) & 1 != 0
}
fn affecth(c: u8) -> bool {
    getcode(c) & 4 != 0
}
fn makesoft(c: u8) -> bool {
    getcode(c) & 8 != 0
}
fn noghtof(c: u8) -> bool {
    getcode(c) & 16 != 0
}

pub(super) fn metaphone(a: &[Value], _: &FnCtx) -> Result<Value> {
    let s = text_arg(&a[0]).as_bytes();
    if s.is_empty() {
        return Ok(Value::text(""));
    }
    if s.len() > MAX_METAPHONE_STRLEN {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("argument exceeds the maximum length of {MAX_METAPHONE_STRLEN} bytes"),
        ));
    }
    let req = a[1].as_i64().unwrap_or(0);
    if req > MAX_METAPHONE_STRLEN as i64 {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("output exceeds the maximum length of {MAX_METAPHONE_STRLEN} bytes"),
        ));
    }
    if req <= 0 {
        return Err(Error::new(code::ZERO_LENGTH_CHARACTER_STRING, "output cannot be empty string"));
    }
    Ok(Value::text(String::from_utf8_lossy(&encode(s, req as usize)).into_owned()))
}

/// `_metaphone` over `word` (a C string: it ends at a NUL).
fn encode(word: &[u8], max: usize) -> Vec<u8> {
    let end = word.iter().position(|&c| c == 0).unwrap_or(word.len());
    let word = &word[..end];
    let at = |i: usize| word.get(i).copied().unwrap_or(0).to_ascii_uppercase();
    let mut out: Vec<u8> = Vec::new();
    let mut w = 0usize;
    // The first letter.
    while !at(w).is_ascii_alphabetic() {
        if at(w) == 0 {
            return out;
        }
        w += 1;
    }
    let next = |w: usize| at(w + 1);
    match at(w) {
        b'A' => {
            if next(w) == b'E' {
                out.push(b'E');
                w += 2;
            } else {
                out.push(b'A');
                w += 1;
            }
        }
        b'G' | b'K' | b'P' => {
            if next(w) == b'N' {
                out.push(b'N');
                w += 2;
            }
        }
        b'W' => {
            if next(w) == b'H' || next(w) == b'R' {
                out.push(next(w));
                w += 2;
            } else if isvowel(next(w)) {
                out.push(b'W');
                w += 2;
            }
        }
        b'X' => {
            out.push(b'S');
            w += 1;
        }
        c @ (b'E' | b'I' | b'O' | b'U') => {
            out.push(c);
            w += 1;
        }
        _ => {}
    }
    while at(w) != 0 && out.len() < max {
        let cur = at(w);
        let back = |n: usize| if w >= n { at(w - n) } else { 0 };
        let after_next = if next(w) != 0 { at(w + 2) } else { 0 };
        // Lookahead: the letter `n` ahead, or the string's end.
        let ahead = |n: usize| {
            let mut i = 0;
            while i < n && at(w + i) != 0 {
                i += 1;
            }
            at(w + i)
        };
        let mut skip = 0;
        if !cur.is_ascii_alphabetic() || (cur == back(1) && cur != b'C') {
            w += 1;
            continue;
        }
        let nx = next(w);
        match cur {
            b'B' => {
                if back(1) != b'M' {
                    out.push(b'B');
                }
            }
            b'C' => {
                if makesoft(nx) {
                    if after_next == b'A' && nx == b'I' {
                        out.push(SH);
                    } else if back(1) == b'S' {
                    } else {
                        out.push(b'S');
                    }
                } else if nx == b'H' {
                    out.push(if after_next == b'R' || back(1) == b'S' { b'K' } else { SH });
                    skip += 1;
                } else {
                    out.push(b'K');
                }
            }
            b'D' => {
                if nx == b'G' && makesoft(after_next) {
                    out.push(b'J');
                    skip += 1;
                } else {
                    out.push(b'T');
                }
            }
            b'G' => {
                if nx == b'H' {
                    if !(noghtof(back(3)) || back(4) == b'H') {
                        out.push(b'F');
                        skip += 1;
                    }
                } else if nx == b'N' {
                    if !after_next.is_ascii_alphabetic() || (after_next == b'E' && ahead(3) == b'D') {
                    } else {
                        out.push(b'K');
                    }
                } else if makesoft(nx) && back(1) != b'G' {
                    out.push(b'J');
                } else {
                    out.push(b'K');
                }
            }
            b'H' => {
                if isvowel(nx) && !affecth(back(1)) {
                    out.push(b'H');
                }
            }
            b'K' => {
                if back(1) != b'C' {
                    out.push(b'K');
                }
            }
            b'P' => out.push(if nx == b'H' { b'F' } else { b'P' }),
            b'Q' => out.push(b'K'),
            b'S' => {
                if nx == b'I' && (after_next == b'O' || after_next == b'A') {
                    out.push(SH);
                } else if nx == b'H' {
                    out.push(SH);
                    skip += 1;
                } else if nx == b'C' && ahead(2) == b'H' && ahead(3) == b'W' {
                    out.push(SH);
                    skip += 2;
                } else {
                    out.push(b'S');
                }
            }
            b'T' => {
                if nx == b'I' && (after_next == b'O' || after_next == b'A') {
                    out.push(SH);
                } else if nx == b'H' {
                    out.push(TH);
                    skip += 1;
                } else {
                    out.push(b'T');
                }
            }
            b'V' => out.push(b'F'),
            b'W' => {
                if isvowel(nx) {
                    out.push(b'W');
                }
            }
            b'X' => {
                out.push(b'K');
                if out.len() < max {
                    out.push(b'S');
                }
            }
            b'Y' => {
                if isvowel(nx) {
                    out.push(b'Y');
                }
            }
            b'Z' => out.push(b'S'),
            b'F' | b'J' | b'L' | b'M' | b'N' | b'R' => out.push(cur),
            _ => {}
        }
        w += skip + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn metaphone_vectors() {
        assert_eq!(super::encode(b"GUMBO", 4), b"KM");
        assert_eq!(super::encode(b"knight", 10), b"NFT");
    }
}
