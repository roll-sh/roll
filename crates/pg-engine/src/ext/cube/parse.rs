//! cube_in: `cubescan.l` and `cubeparse.y`.

use super::{CUBE_MAX_DIM, Ndbox};
use crate::Result;
use crate::error::{Error, code};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Float(String),
    Open,
    Close,
    OBracket,
    CBracket,
    Comma,
    /// Anything else: one character the parser rejects.
    Other(String),
    End,
}

impl Tok {
    /// yytext.
    fn text(&self) -> String {
        match self {
            Tok::Float(s) | Tok::Other(s) => s.clone(),
            Tok::Open => "(".into(),
            Tok::Close => ")".into(),
            Tok::OBracket => "[".into(),
            Tok::CBracket => "]".into(),
            Tok::Comma => ",".into(),
            Tok::End => String::new(),
        }
    }
}

fn digits(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    i
}

/// The longest `{float}` at `i`: `[+-]?({n}|{n}\.{n}?|\.{n})([eE][+-]?{n})?`.
fn float_len(b: &[u8], i: usize) -> usize {
    let mut j = i;
    if j < b.len() && matches!(b[j], b'+' | b'-') {
        j += 1;
    }
    let d = digits(b, j);
    let mut end = if d > j { d } else { 0 };
    if d > j && d < b.len() && b[d] == b'.' {
        end = digits(b, d + 1);
    } else if d == j && j < b.len() && b[j] == b'.' {
        let f = digits(b, j + 1);
        if f > j + 1 {
            end = f;
        }
    }
    if end == 0 {
        return 0;
    }
    if end < b.len() && matches!(b[end], b'e' | b'E') {
        let mut k = end + 1;
        if k < b.len() && matches!(b[k], b'+' | b'-') {
            k += 1;
        }
        let e = digits(b, k);
        if e > k {
            end = e;
        }
    }
    end - i
}

/// `{infinity}` and `{NaN}` at `i`.
fn word_len(b: &[u8], i: usize) -> usize {
    let lower: Vec<u8> = b[i..].iter().map(u8::to_ascii_lowercase).collect();
    if lower.starts_with(b"nan") {
        return 3;
    }
    let s = usize::from(matches!(lower.first(), Some(b'+' | b'-')));
    if lower[s..].starts_with(b"infinity") {
        s + 8
    } else if lower[s..].starts_with(b"inf") {
        s + 3
    } else {
        0
    }
}

fn tokens(s: &str) -> Vec<Tok> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
            i += 1;
            continue;
        }
        let n = float_len(b, i).max(word_len(b, i));
        if n > 0 {
            out.push(Tok::Float(s[i..i + n].to_string()));
            i += n;
            continue;
        }
        out.push(match c {
            b'(' => Tok::Open,
            b')' => Tok::Close,
            b'[' => Tok::OBracket,
            b']' => Tok::CBracket,
            b',' => Tok::Comma,
            // (flex's `.` takes one byte; the message shows it as written.)
            _ => {
                let ch = s[i..].chars().next().unwrap();
                Tok::Other(ch.to_string())
            }
        });
        i += if c < 0x80 { 1 } else { s[i..].chars().next().unwrap().len_utf8() };
    }
    out.push(Tok::End);
    out
}

fn bad(detail: String) -> Error {
    Error::new(code::INVALID_TEXT_REPRESENTATION, "invalid input syntax for cube").detail(detail)
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.at]
    }
    fn syntax(&self) -> Error {
        match self.peek() {
            Tok::End => bad("syntax error at end of input".into()),
            t => bad(format!("syntax error at or near \"{}\"", t.text())),
        }
    }
    fn expect(&mut self, t: Tok) -> Result<()> {
        if *self.peek() != t {
            return Err(self.syntax());
        }
        self.at += 1;
        Ok(())
    }
    /// `list`: floats separated by commas (a comma always continues it).
    fn list(&mut self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        loop {
            match self.peek().clone() {
                Tok::Float(f) => {
                    out.push(f);
                    self.at += 1;
                }
                _ => return Err(self.syntax()),
            }
            if *self.peek() != Tok::Comma {
                return Ok(out);
            }
            self.at += 1;
        }
    }
    /// `paren_list`.
    fn paren_list(&mut self) -> Result<Vec<String>> {
        self.expect(Tok::Open)?;
        if *self.peek() == Tok::Close {
            self.at += 1;
            return Ok(Vec::new());
        }
        let l = self.list()?;
        self.expect(Tok::Close)?;
        Ok(l)
    }
}

/// float8in_internal on one coordinate.
fn coord(s: &str) -> Result<f64> {
    crate::types::text::parse_float8(s)
}

fn too_many() -> Error {
    bad(format!("A cube cannot have more than {CUBE_MAX_DIM} dimensions."))
}

/// `write_box`: corners `a` and `b`, of the same dimensions.
fn write_box(a: &[String], b: &[String]) -> Result<Ndbox> {
    if a.len() != b.len() {
        return Err(bad(format!(
            "Different point dimensions in ({}) and ({}).",
            a.join(","),
            b.join(",")
        )));
    }
    if a.len() > CUBE_MAX_DIM {
        return Err(too_many());
    }
    let mut x = Vec::with_capacity(a.len() * 2);
    for s in a.iter().chain(b) {
        x.push(coord(s)?);
    }
    Ok(Ndbox::boxed(a.len(), x))
}

/// `write_point_as_box`.
fn write_point(a: &[String]) -> Result<Ndbox> {
    if a.len() > CUBE_MAX_DIM {
        return Err(too_many());
    }
    Ok(Ndbox::point(a.iter().map(|s| coord(s)).collect::<Result<_>>()?))
}

/// cube_in: `[(ll),(ur)]`, `(ll),(ur)`, `(x)` or `x, ...`.
pub(crate) fn cube_in(s: &str) -> Result<Ndbox> {
    let mut p = Parser { toks: tokens(s), at: 0 };
    let r = match p.peek() {
        Tok::OBracket => {
            p.at += 1;
            let a = p.paren_list()?;
            p.expect(Tok::Comma)?;
            let b = p.paren_list()?;
            p.expect(Tok::CBracket)?;
            // (The box is reduced before the end is looked at.)
            write_box(&a, &b)?
        }
        Tok::Open => {
            let a = p.paren_list()?;
            if *p.peek() == Tok::Comma {
                p.at += 1;
                let b = p.paren_list()?;
                write_box(&a, &b)?
            } else {
                if *p.peek() != Tok::End {
                    return Err(p.syntax());
                }
                write_point(&a)?
            }
        }
        _ => {
            let a = p.list()?;
            if *p.peek() != Tok::End {
                return Err(p.syntax());
            }
            write_point(&a)?
        }
    };
    if *p.peek() != Tok::End {
        return Err(p.syntax());
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(cube_in("1,2").unwrap(), Ndbox::point(vec![1.0, 2.0]));
        assert_eq!(cube_in("[(1),(1)]").unwrap(), Ndbox::point(vec![1.0]));
        assert!(!cube_in("(1,2),(3,4)").unwrap().point);
        assert_eq!(cube_in("()").unwrap().dim, 0);
        let e = cube_in("(1,2),(3)").unwrap_err();
        assert_eq!(e.detail.as_deref(), Some("Different point dimensions in (1,2) and (3)."));
        assert_eq!(cube_in("1,").unwrap_err().detail.as_deref(), Some("syntax error at end of input"));
        assert_eq!(cube_in("(1),(2),(3)").unwrap_err().detail.as_deref(), Some("syntax error at or near \",\""));
        assert_eq!(cube_in("1 e7").unwrap_err().detail.as_deref(), Some("syntax error at or near \"e\""));
        assert_eq!(cube_in("-infinity").unwrap().x[0], f64::NEG_INFINITY);
    }
}
