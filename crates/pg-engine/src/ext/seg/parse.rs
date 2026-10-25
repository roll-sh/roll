//! seg_in: `segscan.l` and `segparse.y`.

use super::Seg;
use super::restore::c_g;
use crate::Result;
use crate::error::{Error, code};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Float(String),
    Range,
    Plumin,
    Ext(u8),
    Other(String),
    End,
}

fn token_at(s: &str, i: usize) -> (Tok, usize) {
    let b = s.as_bytes();
    let rest = &b[i..];
    if rest.starts_with(b"...") {
        return (Tok::Range, 3);
    }
    if rest.starts_with(b"..") {
        return (Tok::Range, 2);
    }
    if rest.starts_with(b"'+-'") || rest.starts_with(b"(+-)") {
        return (Tok::Plumin, 4);
    }
    // {float}: [+-]?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?
    let digits = |mut j: usize| {
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        j
    };
    let mut j = i + usize::from(matches!(rest.first(), Some(b'+' | b'-')));
    let d = digits(j);
    if d > j {
        j = d;
        if j + 1 < b.len() && b[j] == b'.' && b[j + 1].is_ascii_digit() {
            j = digits(j + 1);
        }
        if j < b.len() && matches!(b[j], b'e' | b'E') {
            let mut k = j + 1;
            if k < b.len() && matches!(b[k], b'+' | b'-') {
                k += 1;
            }
            let e = digits(k);
            if e > k {
                j = e;
            }
        }
        return (Tok::Float(s[i..j].to_string()), j - i);
    }
    match rest[0] {
        c @ (b'<' | b'>' | b'~') => (Tok::Ext(c), 1),
        _ => {
            let ch = s[i..].chars().next().unwrap();
            (Tok::Other(ch.to_string()), ch.len_utf8())
        }
    }
}

fn tokens(s: &str) -> Vec<(Tok, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        if matches!(s.as_bytes()[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
            i += 1;
            continue;
        }
        let (t, n) = token_at(s, i);
        out.push((t, s[i..i + n].to_string()));
        i += n;
    }
    out.push((Tok::End, String::new()));
    out
}

fn syntax(text: &str, end: bool) -> Error {
    let detail = if end {
        "syntax error at end of input".to_string()
    } else {
        format!("syntax error at or near \"{text}\"")
    };
    Error::new(code::SYNTAX_ERROR, "bad seg representation").detail(detail)
}

/// `significant_digits`.
pub(crate) fn significant_digits(s: &str) -> i32 {
    let b = s.as_bytes();
    let mut p = 0;
    while p < b.len() && matches!(b[p], b'0' | b'+' | b'-') {
        p += 1;
    }
    let mut zeroes = 1;
    while p < b.len() && matches!(b[p], b'0' | b'.') {
        if b[p] != b'.' {
            zeroes += 1;
        }
        p += 1;
    }
    let mut n = 0;
    while p < b.len() && (b[p].is_ascii_digit() || b[p] == b'.') {
        if b[p] != b'.' {
            n += 1;
        }
        p += 1;
    }
    if n == 0 { zeroes } else { n }
}

fn sig_digits(s: &str) -> u8 {
    significant_digits(s).min(6) as u8
}

/// A boundary: (value, ext, significant digits).
type Bnd = (f32, u8, u8);

struct Parser {
    toks: Vec<(Tok, String)>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.at].0
    }
    fn error(&self) -> Error {
        let (t, text) = &self.toks[self.at];
        syntax(text, *t == Tok::End)
    }
    fn float(&mut self) -> Result<(f32, u8)> {
        match self.peek().clone() {
            Tok::Float(f) => {
                self.at += 1;
                Ok((crate::types::text::parse_float4(&f)?, sig_digits(&f)))
            }
            _ => Err(self.error()),
        }
    }
    fn boundary(&mut self) -> Result<Bnd> {
        let ext = match self.peek() {
            Tok::Ext(c) => {
                let c = *c;
                self.at += 1;
                c
            }
            _ => 0,
        };
        let (v, sigd) = self.float()?;
        Ok((v, ext, sigd))
    }
    fn starts_boundary(&self) -> bool {
        matches!(self.peek(), Tok::Float(_) | Tok::Ext(_))
    }
}

/// seg_in.
pub(crate) fn seg_in(s: &str) -> Result<Seg> {
    let mut p = Parser { toks: tokens(s), at: 0 };
    let seg = if *p.peek() == Tok::Range {
        p.at += 1;
        let (v, ext, sigd) = p.boundary()?;
        Seg { lower: f32::NEG_INFINITY, upper: v, l_sigd: 0, u_sigd: sigd, l_ext: b'-', u_ext: ext }
    } else {
        let (v, ext, sigd) = p.boundary()?;
        match p.peek() {
            Tok::Plumin => {
                p.at += 1;
                let (d, dsigd) = p.float()?;
                let (lower, upper) = (v - d, v + d);
                let m = sigd.max(dsigd);
                Seg {
                    lower,
                    upper,
                    l_sigd: sig_digits(&c_g(lower as f64)).max(m),
                    u_sigd: sig_digits(&c_g(upper as f64)).max(m),
                    l_ext: 0,
                    u_ext: 0,
                }
            }
            Tok::Range => {
                p.at += 1;
                if p.starts_boundary() {
                    let (u, uext, usigd) = p.boundary()?;
                    if v > u {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            format!("swapped boundaries: {} is greater than {}", c_g(v as f64), c_g(u as f64)),
                        ));
                    }
                    Seg { lower: v, upper: u, l_sigd: sigd, u_sigd: usigd, l_ext: ext, u_ext: uext }
                } else {
                    if *p.peek() != Tok::End {
                        return Err(p.error());
                    }
                    Seg { lower: v, upper: f32::INFINITY, l_sigd: sigd, u_sigd: 0, l_ext: ext, u_ext: b'-' }
                }
            }
            Tok::End => Seg { lower: v, upper: v, l_sigd: sigd, u_sigd: sigd, l_ext: ext, u_ext: ext },
            _ => return Err(p.error()),
        }
    };
    if *p.peek() != Tok::End {
        return Err(p.error());
    }
    Ok(seg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(s: &str) -> String {
        let b = seg_in(s).unwrap().to_bytes();
        super::super::output(&b, &crate::types::text::FormatCtx::default())
    }

    #[test]
    fn round_trips() {
        assert_eq!(out("5.0"), "5.0");
        assert_eq!(out("-1e7"), "-1e+07");
        assert_eq!(out("1 .. 2"), "1 .. 2");
        assert_eq!(out("<1.1 .."), "<1.1 ..");
        assert_eq!(out(".. >2"), ".. >2");
        assert_eq!(out("1(+-)0.5"), "0.5 .. 1.5");
        assert_eq!(significant_digits("0.0012"), 2);
        assert_eq!(significant_digits("000"), 1);
        assert_eq!(significant_digits("0.000"), 4);
        let e = seg_in("2 .. 1").unwrap_err();
        assert_eq!(e.message, "swapped boundaries: 2 is greater than 1");
        assert_eq!(seg_in("1 2").unwrap_err().detail.as_deref(), Some("syntax error at or near \"2\""));
        assert_eq!(seg_in("").unwrap_err().detail.as_deref(), Some("syntax error at end of input"));
    }
}
