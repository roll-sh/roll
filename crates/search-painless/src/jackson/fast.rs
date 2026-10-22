//! A byte-level reader for well-formed, unremarkable JSON, the shape of nearly every stored
//! source and `Json.load` argument. It yields exactly the values the exact reader would
//! (`Integer`/`Long` by digit count, `Double`, `String`, `Boolean`, null, maps of the requested
//! class, `ArrayList`s), or `None` at the first thing Jackson treats specially: comments,
//! duplicate keys, leading zeros, integers beyond 18 digits, lone surrogates, control
//! characters, unknown escapes, deep nesting, trailing content, or any syntax error. The caller
//! then runs the exact reader, which produces Jackson's errors and frames.
//!
//! It works on UTF-8 bytes directly: strings without escapes are one `memchr`-like scan and one
//! copy, numbers are parsed from the source slice, and there is no UTF-16 transcoding.

use std::rc::Rc;

use crate::types::ClassId;
use crate::value::{JList, JMap, Value};

/// Nesting handled here; deeper input goes to the exact reader.
const MAX_DEPTH: usize = 256;

/// Longest number text parsed here (Jackson's own limit is 1,000).
const MAX_NUMBER_LEN: usize = 300;

struct P<'a> {
    src: &'a str,
    b: &'a [u8],
    i: usize,
    map_class: ClassId,
    depth: usize,
}

/// The value of `b` if it is one plain JSON value with nothing after it but whitespace.
pub(super) fn load(src: &str, map_class: ClassId) -> Option<Value> {
    let mut p = P { src, b: src.as_bytes(), i: 0, map_class, depth: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    (p.i == src.len()).then_some(v)
}

impl<'a> P<'a> {
    #[inline]
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    #[inline]
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Option<Value> {
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Value::string),
            b't' => self.word(b"true", Value::Bool(true)),
            b'f' => self.word(b"false", Value::Bool(false)),
            b'n' => self.word(b"null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn word(&mut self, w: &[u8], v: Value) -> Option<Value> {
        if self.b.get(self.i..self.i + w.len())? != w {
            return None;
        }
        self.i += w.len();
        // `truex` is one unrecognized token for Jackson
        if self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80) {
            return None;
        }
        Some(v)
    }

    fn enter(&mut self) -> Option<()> {
        self.depth += 1;
        self.i += 1;
        self.ws();
        (self.depth <= MAX_DEPTH).then_some(())
    }

    /// After an element: `true` at `,`, `false` at `close`.
    #[inline]
    fn separator(&mut self, close: u8) -> Option<bool> {
        self.ws();
        let c = self.peek()?;
        self.i += 1;
        if c == b',' {
            self.ws();
            Some(true)
        } else if c == close {
            Some(false)
        } else {
            None
        }
    }

    fn object(&mut self) -> Option<Value> {
        self.enter()?;
        let m = JMap::new(self.map_class);
        if self.peek() == Some(b'}') {
            self.i += 1;
        } else {
            loop {
                if self.peek() != Some(b'"') {
                    return None;
                }
                let k = self.string()?;
                self.ws();
                if self.peek() != Some(b':') {
                    return None;
                }
                self.i += 1;
                self.ws();
                let v = self.value()?;
                // duplicate keys are Jackson's error to report
                if m.insert_raw(Value::string(k), v).is_some() {
                    return None;
                }
                if !self.separator(b'}')? {
                    break;
                }
            }
        }
        self.depth -= 1;
        Some(Value::Map(Rc::new(m)))
    }

    fn array(&mut self) -> Option<Value> {
        self.enter()?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.i += 1;
        } else {
            loop {
                items.push(self.value()?);
                if !self.separator(b']')? {
                    break;
                }
            }
        }
        self.depth -= 1;
        Some(JList::array_list(items))
    }

    /// Skips the bytes up to the next quote, backslash or control character and returns them.
    /// The input is a `str` and the run stops only at ASCII bytes, so it is whole characters.
    #[inline]
    fn run(&mut self) -> &'a str {
        let start = self.i;
        let n = self.b[start..].iter().position(|&c| c == b'"' || c == b'\\' || c < 0x20).unwrap_or(self.b.len() - start);
        self.i += n;
        let src: &'a str = self.src;
        &src[start..start + n]
    }

    fn string(&mut self) -> Option<String> {
        self.i += 1;
        let mut out = self.run().to_string();
        loop {
            match self.peek()? {
                b'"' => {
                    self.i += 1;
                    return Some(out);
                }
                b'\\' => {
                    let e = *self.b.get(self.i + 1)?;
                    self.i += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let u = self.hex4()?;
                            let c = if (0xD800..0xDC00).contains(&u) {
                                if self.b.get(self.i..self.i + 2)? != b"\\u" {
                                    return None;
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return None;
                                }
                                char::from_u32(0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00))?
                            } else {
                                char::from_u32(u)?
                            };
                            out.push(c);
                        }
                        _ => return None,
                    }
                }
                0..=0x1f => return None,
                _ => {
                    let s = self.run();
                    out.push_str(s);
                }
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = self.b.get(self.i..self.i + 4)?;
        let mut v = 0;
        for &c in h {
            v = v * 16 + (c as char).to_digit(16)?;
        }
        self.i += 4;
        Some(v)
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let int_start = self.i;
        let digits = self.digits();
        // bare signs and leading zeros are Jackson's to report
        if digits == 0 || (digits > 1 && self.b[int_start] == b'0') {
            return None;
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            float = true;
            self.i += 1;
            if self.digits() == 0 {
                return None;
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            float = true;
            self.i += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.i += 1;
            }
            if self.digits() == 0 {
                return None;
            }
        }
        // `1x` is one bad token for Jackson
        if self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c >= 0x80) {
            return None;
        }
        if self.i - start > MAX_NUMBER_LEN {
            return None;
        }
        let text = &self.src[start..self.i];
        if float {
            return text.parse().ok().map(Value::Double);
        }
        if digits > 18 {
            return None;
        }
        let l: i64 = text.parse().ok()?;
        Some(match i32::try_from(l) {
            Ok(i) if digits <= 10 => Value::Int(i),
            _ => Value::Long(l),
        })
    }
}
