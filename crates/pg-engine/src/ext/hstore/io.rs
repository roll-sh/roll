//! hstore's text input and output (hstore_io.c): `parse_hstore`'s state machines, `hstore_out`.

use super::{Hs, MAX_LEN, Pair};
use crate::error::{Error, Result, code};

/// `scanner_isspace`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\x0b')
}

struct Parser<'a> {
    /// The input with a terminating NUL, as the C code reads it.
    s: Vec<u8>,
    text: &'a str,
    ptr: usize,
    word: Vec<u8>,
}

impl Parser<'_> {
    fn at(&self) -> u8 {
        self.s.get(self.ptr).copied().unwrap_or(0)
    }

    /// `prssyntaxerror`: the character at the cursor (a whole multibyte one) and its byte
    /// position.
    fn syntax_error(&self) -> Error {
        let rest = self.text.get(self.ptr..).unwrap_or("");
        let ch: String = rest.chars().next().map(String::from).unwrap_or_default();
        Error::new(
            code::SYNTAX_ERROR,
            format!("syntax error in hstore, near \"{ch}\" at position {}", self.ptr),
        )
    }

    fn eof_error(&self) -> Error {
        Error::new(code::SYNTAX_ERROR, "syntax error in hstore: unexpected end of string")
    }

    /// `get_val`: the next key or value into `word`; Ok(false) at the end of the input.
    /// Returns whether it was quoted too.
    fn get_val(&mut self, ignoreeq: bool) -> Result<Option<bool>> {
        #[derive(PartialEq)]
        enum St {
            WaitVal,
            InVal,
            InEscVal,
            WaitEscIn,
            WaitEscEscIn,
        }
        let mut st = St::WaitVal;
        let mut escaped = false;
        self.word.clear();
        loop {
            let c = self.at();
            match st {
                St::WaitVal => {
                    if c == b'"' {
                        escaped = true;
                        st = St::InEscVal;
                    } else if c == 0 {
                        return Ok(None);
                    } else if c == b'=' && !ignoreeq {
                        return Err(self.syntax_error());
                    } else if c == b'\\' {
                        st = St::WaitEscIn;
                    } else if !is_space(c) {
                        self.word.push(c);
                        st = St::InVal;
                    }
                }
                St::InVal => {
                    if c == b'\\' {
                        st = St::WaitEscIn;
                    } else if (c == b'=' && !ignoreeq) || (c == b',' && ignoreeq) || c == 0 {
                        self.ptr -= 1;
                        return Ok(Some(escaped));
                    } else if is_space(c) {
                        return Ok(Some(escaped));
                    } else {
                        self.word.push(c);
                    }
                }
                St::InEscVal => {
                    if c == b'\\' {
                        st = St::WaitEscEscIn;
                    } else if c == b'"' {
                        return Ok(Some(escaped));
                    } else if c == 0 {
                        return Err(self.eof_error());
                    } else {
                        self.word.push(c);
                    }
                }
                St::WaitEscIn | St::WaitEscEscIn => {
                    if c == 0 {
                        return Err(self.eof_error());
                    }
                    self.word.push(c);
                    st = if st == St::WaitEscIn { St::InVal } else { St::InEscVal };
                }
            }
            self.ptr += 1;
        }
    }

    /// `parse_hstore`.
    fn parse(&mut self) -> Result<Vec<Pair>> {
        #[derive(PartialEq)]
        enum St {
            Key,
            Val,
            Eq,
            Gt,
            Del,
        }
        let mut st = St::Key;
        let mut pairs: Vec<Pair> = Vec::new();
        let mut key = Vec::new();
        loop {
            match st {
                St::Key => {
                    if self.get_val(false)?.is_none() {
                        return Ok(pairs);
                    }
                    check_key_len(self.word.len())?;
                    key = std::mem::take(&mut self.word);
                    st = St::Eq;
                }
                St::Eq => {
                    let c = self.at();
                    if c == b'=' {
                        st = St::Gt;
                    } else if c == 0 {
                        return Err(self.eof_error());
                    } else if !is_space(c) {
                        return Err(self.syntax_error());
                    }
                }
                St::Gt => {
                    let c = self.at();
                    if c == b'>' {
                        st = St::Val;
                    } else if c == 0 {
                        return Err(self.eof_error());
                    } else {
                        return Err(self.syntax_error());
                    }
                }
                St::Val => {
                    let Some(escaped) = self.get_val(true)? else {
                        return Err(self.eof_error());
                    };
                    check_val_len(self.word.len())?;
                    let val = std::mem::take(&mut self.word);
                    let null = !escaped && val.len() == 4 && val.eq_ignore_ascii_case(b"null");
                    pairs.push(Pair {
                        key: std::mem::take(&mut key),
                        val: (!null).then_some(val),
                        needfree: true,
                    });
                    st = St::Del;
                }
                St::Del => {
                    let c = self.at();
                    if c == b',' {
                        st = St::Key;
                    } else if c == 0 {
                        return Ok(pairs);
                    } else if !is_space(c) {
                        return Err(self.syntax_error());
                    }
                }
            }
            self.ptr += 1;
        }
    }
}

/// `hstoreCheckKeyLen`.
pub(crate) fn check_key_len(n: usize) -> Result<()> {
    if n > MAX_LEN {
        return Err(Error::new(code::STRING_DATA_RIGHT_TRUNCATION, "string too long for hstore key"));
    }
    Ok(())
}

/// `hstoreCheckValLen`.
pub(crate) fn check_val_len(n: usize) -> Result<()> {
    if n > MAX_LEN {
        return Err(Error::new(code::STRING_DATA_RIGHT_TRUNCATION, "string too long for hstore value"));
    }
    Ok(())
}

/// `hstore_in`.
pub(super) fn input(text: &str) -> Result<Vec<u8>> {
    let mut s = text.as_bytes().to_vec();
    s.push(0);
    let mut p = Parser {
        s,
        text,
        ptr: 0,
        word: Vec::new(),
    };
    let pairs = p.parse()?;
    Ok(super::from_pairs(pairs))
}

/// `cpw`: a string with `"` and `\` escaped.
fn cpw(out: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        if c == b'"' || c == b'\\' {
            out.push(b'\\');
        }
        out.push(c);
    }
}

/// `hstore_out`.
pub(super) fn output(b: &[u8]) -> String {
    let h = Hs::new(b);
    let mut out = Vec::new();
    for (i, (k, v)) in h.pairs().enumerate() {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        out.push(b'"');
        cpw(&mut out, k);
        out.extend_from_slice(b"\"=>");
        match v {
            Some(v) => {
                out.push(b'"');
                cpw(&mut out, v);
                out.push(b'"');
            }
            None => out.extend_from_slice(b"NULL"),
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}
