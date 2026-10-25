//! ltree's `ltree` and `lquery` (`ltree_io.c`): parsing, output and the bytes values keep.
//! An ltree is its labels (u16 length and bytes each); an lquery its levels and variants.

use crate::error::{Error, Result, code};

pub const LTREE_MAX_LEVELS: usize = 65535;
const LABEL_MAX_CHARS: i64 = 1000;

pub const LVAR_ANYEND: u16 = 0x01;
pub const LVAR_INCASE: u16 = 0x02;
pub const LVAR_SUBLEXEME: u16 = 0x04;
pub const LQL_NOT: u16 = 0x10;
pub const LQL_COUNT: u16 = 0x20;

/// ISLABEL: alphanumerics (the C locale's, ASCII), `_` and `-`.
pub fn is_label(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

pub fn encode(labels: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in labels {
        out.extend_from_slice(&(l.len() as u16).to_le_bytes());
        out.extend_from_slice(l);
    }
    out
}

pub fn labels(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 <= b.len() {
        let n = u16::from_le_bytes([b[i], b[i + 1]]) as usize;
        out.push(&b[i + 2..i + 2 + n]);
        i += 2 + n;
    }
    out
}

pub fn output(b: &[u8]) -> String {
    let l: Vec<String> = labels(b).iter().map(|x| String::from_utf8_lossy(x).into_owned()).collect();
    l.join(".")
}

/// `ltree_compare`.
pub fn compare(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let (x, y) = (labels(a), labels(b));
    for (p, q) in x.iter().zip(&y) {
        let n = p.len().min(q.len());
        match p[..n].cmp(&q[..n]).then(p.len().cmp(&q.len())) {
            std::cmp::Ordering::Equal => {}
            o => return o,
        }
    }
    x.len().cmp(&y.len())
}

/// An order-preserving key: each label's bytes, escaped, then a terminator; an end marker
/// below any label.
pub fn key(a: &[u8], out: &mut Vec<u8>) {
    for l in labels(a) {
        super::super::types::key_bytes(l, out);
    }
    out.extend_from_slice(&[0x00, 0x00]);
}

/// A label being parsed: where it starts (byte offset), its flags and its length in chars.
struct Node {
    start: usize,
    end: usize,
    wlen: i64,
}

fn syntax(what: &str, pos: i64) -> Error {
    Error::new(code::SYNTAX_ERROR, format!("{what} syntax error at character {pos}"))
}

/// `finish_nodeitem`: the label ends at byte `at` (flag characters before it, of an lquery,
/// aren't part of it).
fn finish(s: &str, n: &mut Node, mut at: usize, lquery: bool, mut pos: i64) -> Result<()> {
    let b = s.as_bytes();
    if lquery {
        while at > n.start && matches!(b[at - 1], b'@' | b'*' | b'%') {
            at -= 1;
            n.wlen -= 1;
            pos -= 1;
        }
    }
    n.end = at;
    let what = if lquery { "lquery" } else { "ltree" };
    if n.end == n.start {
        return Err(syntax(what, pos).detail("Empty labels are not allowed."));
    }
    if n.wlen > LABEL_MAX_CHARS {
        return Err(Error::new(code::NAME_TOO_LONG, "label string is too long").detail(format!(
            "Label length is {}, must be at most {LABEL_MAX_CHARS}, at character {pos}.",
            n.wlen
        )));
    }
    Ok(())
}

/// `parse_ltree`.
pub fn parse(s: &str) -> Result<Vec<u8>> {
    let num = s.chars().filter(|&c| c == '.').count();
    if num + 1 > LTREE_MAX_LEVELS {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            format!("number of ltree labels ({}) exceeds the maximum allowed ({LTREE_MAX_LEVELS})", num + 1),
        ));
    }
    let mut list: Vec<Node> = Vec::new();
    let mut cur: Option<Node> = None;
    let mut pos: i64 = 1;
    for (i, c) in s.char_indices() {
        match &mut cur {
            None => {
                if !is_label(c) {
                    return Err(syntax("ltree", pos));
                }
                cur = Some(Node { start: i, end: i, wlen: 0 });
            }
            Some(n) => {
                if c == '.' {
                    finish(s, n, i, false, pos)?;
                    list.push(cur.take().unwrap());
                    pos += 1;
                    continue;
                } else if !is_label(c) {
                    return Err(syntax("ltree", pos));
                }
            }
        }
        if let Some(n) = &mut cur {
            n.wlen += 1;
        }
        pos += 1;
    }
    match cur {
        Some(mut n) => {
            finish(s, &mut n, s.len(), false, pos)?;
            list.push(n);
        }
        None if list.is_empty() => {}
        None => {
            return Err(Error::new(code::SYNTAX_ERROR, "ltree syntax error").detail("Unexpected end of input."));
        }
    }
    let b = s.as_bytes();
    Ok(encode(&list.iter().map(|n| &b[n.start..n.end]).collect::<Vec<_>>()))
}
