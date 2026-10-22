//! A JSON parser that fails the way Elasticsearch's Jackson parser fails.
//!
//! Elasticsearch returns Jackson's message, line and column verbatim in
//! `x_content_parse_exception` (and `x_content_e_o_f_exception` for truncated input), and the
//! YAML REST tests and clients match on them, so this parser mirrors Jackson's
//! `UTF8StreamJsonParser` decisions: which state reports which message, which errors are
//! `JsonEOFException`s, and where each error is located (columns are 1-based byte offsets in the
//! line). It also accepts what Elasticsearch's Jackson configuration accepts: `/* */` and `//`
//! comments, and any trailing content after the first value. Duplicate keys are rejected
//! (`STRICT_DUPLICATE_DETECTION`). Expected outputs live in `tests/fixtures/json_errors.json`.
//!
//! The parser works on bytes without a tokenizer layer and allocates only the strings it
//! returns. Nesting is limited to Jackson's default of 1000 levels so recursion is bounded.

use super::{Map, Number, Value};
use std::fmt::Write as _;

const MAX_DEPTH: usize = 1000;
const MAX_NUMBER_LEN: usize = 1000;
/// Jackson includes at most this many bytes of the input in a location description.
const MAX_SOURCE_EXCERPT: usize = 500;

const VALID_VALUES: &str = "(JSON String, Number, Array, Object or token 'null', 'true' or 'false')";

/// Jackson's description of the input in error locations: `(byte[])"…"` with control
/// characters escaped and long input truncated.
pub fn source_desc(s: &[u8]) -> String {
    let n = s.len().min(MAX_SOURCE_EXCERPT);
    let text = String::from_utf8_lossy(&s[..n]);
    let mut out = String::with_capacity(n + 16);
    out.push_str("(byte[])\"");
    for ch in text.chars() {
        let cp = ch as u32;
        let iso_control = cp < 0x20 || (0x7f..=0x9f).contains(&cp);
        if iso_control && ch != '\r' && ch != '\n' {
            let _ = write!(out, "\\u{cp:04X}");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    if s.len() > MAX_SOURCE_EXCERPT {
        let _ = write!(out, "[truncated {} bytes]", s.len() - MAX_SOURCE_EXCERPT);
    }
    out
}

/// Jackson's location description: `[Source: (byte[])"…"; line: 1, column: 2]`.
pub fn source_location(s: &[u8], line: usize, column: usize) -> String {
    format!("[Source: {}; line: {line}, column: {column}]", source_desc(s))
}

/// A parse failure with Jackson's message and location.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonError {
    /// `true` for Jackson's `JsonEOFException` (input ended early).
    pub eof: bool,
    /// The message without the location suffix.
    pub message: String,
    pub line: usize,
    pub column: usize,
    /// The location suffix Jackson appends: `\n at [Source: (byte[])"…"; line: 1, column: 2]`.
    pub location: String,
}

impl JsonError {
    /// Jackson's full exception message.
    pub fn full_message(&self) -> String {
        format!("{}{}", self.message, self.location)
    }
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.full_message())
    }
}

/// Parses the first JSON value in `input`; anything after it is ignored, as in Elasticsearch.
/// Empty (or whitespace-only) input yields `Ok(None)`.
pub fn parse(input: &[u8]) -> Result<Option<Value>, JsonError> {
    let mut p = Parser { s: input, pos: 0, line: 1, row_start: 0, depth: 0 };
    p.skip_ws_or_eof()?;
    if p.pos >= p.s.len() {
        return Ok(None);
    }
    let c = p.s[p.pos];
    match c {
        b'}' => {
            p.pos += 1;
            Err(p.err_prev("Unexpected close marker '}': no open Object to close".to_string()))
        }
        b']' => {
            p.pos += 1;
            Err(p.err_prev("Unexpected close marker ']': no open Array to close".to_string()))
        }
        _ => p.root().map(Some),
    }
}

/// Parses a body that must be an object. A body whose first token isn't an object yields an
/// empty map, as Elasticsearch's map readers do.
pub fn parse_object(input: &[u8]) -> Result<Map, JsonError> {
    // Elasticsearch's map readers stop at a leading `[` without reading further.
    if input.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'[') {
        return Ok(Map::new());
    }
    match parse(input)? {
        Some(Value::Obj(m)) => Ok(m),
        _ => Ok(Map::new()),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Root,
    Object,
    Array,
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
    line: usize,
    row_start: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn source(&self) -> String {
        source_desc(self.s)
    }

    fn loc_desc(&self, line: usize, column: usize) -> String {
        format!("[Source: {}; line: {line}, column: {column}]", self.source())
    }

    fn make(&self, eof: bool, message: String, line: usize, column: usize) -> JsonError {
        let location = format!("\n at {}", self.loc_desc(line, column));
        JsonError { eof, message, line, column, location }
    }

    /// An error located at the byte just consumed (Jackson's `_currentLocationMinusOne`).
    fn err_prev(&self, message: String) -> JsonError {
        self.make(false, message, self.line, self.pos - self.row_start)
    }

    /// An error located at the next unread byte (Jackson's current location).
    fn err_here(&self, message: String) -> JsonError {
        self.make(false, message, self.line, self.pos - self.row_start + 1)
    }

    fn eof_err(&self, message: String) -> JsonError {
        self.make(true, message, self.line, self.pos - self.row_start + 1)
    }

    fn eof_in_ctx(&self, ctx: Ctx, open: (usize, usize)) -> JsonError {
        let what = if ctx == Ctx::Array { "Array" } else { "Object" };
        let m = format!("Unexpected end-of-input: expected close marker for {what} (start marker at {})", self.loc_desc(open.0, open.1));
        self.eof_err(m)
    }

    /// Skips whitespace and comments; `Ok` at end of input too.
    fn skip_ws_or_eof(&mut self) -> Result<(), JsonError> {
        while self.pos < self.s.len() {
            let c = self.s[self.pos];
            match c {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => {
                    self.pos += 1;
                    self.line += 1;
                    self.row_start = self.pos;
                }
                b'/' => {
                    self.pos += 1;
                    self.skip_comment()?;
                }
                c if c < 0x20 => {
                    self.pos += 1;
                    return Err(self.err_prev(format!(
                        "Illegal character ((CTRL-CHAR, code {c})): only regular white space (\\r, \\n, \\t) is allowed between tokens"
                    )));
                }
                _ => return Ok(()),
            }
        }
        Ok(())
    }

    fn skip_comment(&mut self) -> Result<(), JsonError> {
        if self.pos >= self.s.len() {
            return Err(self.eof_err("Unexpected end-of-input in a comment".to_string()));
        }
        let c = self.s[self.pos];
        self.pos += 1;
        match c {
            b'*' => loop {
                if self.pos >= self.s.len() {
                    return Err(self.eof_err("Unexpected end-of-input in a comment".to_string()));
                }
                let c = self.s[self.pos];
                self.pos += 1;
                match c {
                    b'*' if self.s.get(self.pos) == Some(&b'/') => {
                        self.pos += 1;
                        return Ok(());
                    }
                    b'\n' => {
                        self.line += 1;
                        self.row_start = self.pos;
                    }
                    _ => {}
                }
            },
            b'/' => {
                while self.pos < self.s.len() {
                    let c = self.s[self.pos];
                    self.pos += 1;
                    if c == b'\n' {
                        self.line += 1;
                        self.row_start = self.pos;
                        break;
                    }
                }
                Ok(())
            }
            _ => {
                let d = self.char_desc_at(self.pos - 1);
                Err(self.err_prev(format!("Unexpected character ({d}): was expecting either '*' or '/' for a comment")))
            }
        }
    }

    /// Jackson's `_getCharDesc` of the (possibly multi-byte) character starting at `at`.
    fn char_desc_at(&self, at: usize) -> String {
        let cp = decode_char(&self.s[at..]);
        char_desc(cp)
    }

    fn unexpected(&mut self, msg: &str) -> JsonError {
        // The offending character has been consumed.
        let start = self.pos;
        let d = self.char_desc_at(start);
        self.pos = start + utf8_len(self.s[start]).min(self.s.len() - start).max(1);
        self.err_prev_at(start, format!("Unexpected character ({d}): {msg}"))
    }

    /// Error located at the (1-based) column of the byte at `at`.
    fn err_prev_at(&self, at: usize, message: String) -> JsonError {
        self.make(false, message, self.line, at - self.row_start + 1)
    }

    fn value(&mut self, ctx: Ctx) -> Result<Value, JsonError> {
        let c = self.s[self.pos];
        match c {
            b'"' => {
                self.pos += 1;
                self.string(false).map(Value::Str)
            }
            b't' => self.keyword("true", Value::Bool(true)),
            b'f' => self.keyword("false", Value::Bool(false)),
            b'n' => self.keyword("null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            b'+' => {
                self.pos += 2.min(self.s.len() - self.pos);
                Err(self.err_prev(
                    "Unexpected character ('+' (code 43)) in numeric value: JSON spec does not allow numbers to have plus signs: enable `JsonReadFeature.ALLOW_LEADING_PLUS_SIGN_FOR_NUMBERS` to allow"
                        .to_string(),
                ))
            }
            b'}' => Err(self.unexpected("expected a value")),
            b',' if ctx != Ctx::Root => Err(self.unexpected("expected a value")),
            b']' if ctx == Ctx::Array => Err(self.unexpected("expected a value")),
            _ => {
                let cp = decode_char(&self.s[self.pos..]);
                if char::from_u32(cp).is_some_and(is_java_identifier_start) {
                    let start = self.pos;
                    let tok = self.identifier();
                    if tok == "NaN" || tok == "Infinity" {
                        return Err(self.err_here(format!(
                            "Non-standard token '{tok}': enable `JsonReadFeature.ALLOW_NON_NUMERIC_NUMBERS` to allow"
                        )));
                    }
                    return Err(self.err_prev_at(start, format!("Unrecognized token '{tok}': was expecting {VALID_VALUES}")));
                }
                Err(self.unexpected(&format!("expected a valid value {VALID_VALUES}")))
            }
        }
    }

    /// Consumes identifier-part characters from `pos` (for "Unrecognized token" messages).
    fn identifier(&mut self) -> String {
        let start = self.pos;
        while self.pos < self.s.len() {
            let b = self.s[self.pos];
            let len = utf8_len(b).max(1);
            let cp = decode_char(&self.s[self.pos..]);
            if !char::from_u32(cp).is_some_and(is_java_identifier_part) {
                break;
            }
            self.pos += len.min(self.s.len() - self.pos);
        }
        String::from_utf8_lossy(&self.s[start..self.pos]).into_owned()
    }

    fn keyword(&mut self, word: &'static str, v: Value) -> Result<Value, JsonError> {
        let start = self.pos;
        let w = word.as_bytes();
        let mut i = 0;
        while i < w.len() && self.pos + i < self.s.len() && self.s[self.pos + i] == w[i] {
            i += 1;
        }
        let complete = i == w.len();
        let next_is_ident = self.pos + i < self.s.len() && {
            let cp = decode_char(&self.s[self.pos + i..]);
            char::from_u32(cp).is_some_and(is_java_identifier_part)
        };
        if complete && !next_is_ident {
            self.pos += i;
            return Ok(v);
        }
        let tok = self.identifier();
        Err(self.err_prev_at(start, format!("Unrecognized token '{tok}': was expecting {VALID_VALUES}")))
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        let neg = self.s[self.pos] == b'-';
        if neg {
            self.pos += 1;
            match self.s.get(self.pos) {
                None => return Err(self.eof_err("Unexpected end-of-input in a Number value".to_string())),
                Some(b'0'..=b'9') => {}
                Some(b'I') => {
                    let tok = self.identifier();
                    if tok == "Infinity" {
                        return Err(self.err_here(
                            "Non-standard token '-Infinity': enable `JsonReadFeature.ALLOW_NON_NUMERIC_NUMBERS` to allow".to_string(),
                        ));
                    }
                    self.pos = start + 1;
                    return Err(self.number_char_err("expected digit (0-9) to follow minus sign, for valid numeric value"));
                }
                Some(_) => return Err(self.number_char_err("expected digit (0-9) to follow minus sign, for valid numeric value")),
            }
        }
        let int_start = self.pos;
        if self.s[self.pos] == b'0' {
            self.pos += 1;
            if let Some(b'0'..=b'9') = self.s.get(self.pos) {
                self.pos += 1;
                return Err(self.err_prev("Invalid numeric value: Leading zeroes not allowed".to_string()));
            }
        } else {
            while let Some(b'0'..=b'9') = self.s.get(self.pos) {
                self.pos += 1;
            }
        }
        let int_end = self.pos;
        let mut float = false;
        if self.s.get(self.pos) == Some(&b'.') {
            float = true;
            self.pos += 1;
            match self.s.get(self.pos) {
                Some(b'0'..=b'9') => {}
                None => return Err(self.eof_err("Unexpected end-of-input in a Number value".to_string())),
                Some(_) => return Err(self.number_char_err("Decimal point not followed by a digit")),
            }
            while let Some(b'0'..=b'9') = self.s.get(self.pos) {
                self.pos += 1;
            }
        }
        if let Some(b'e' | b'E') = self.s.get(self.pos) {
            float = true;
            self.pos += 1;
            if let Some(b'+' | b'-') = self.s.get(self.pos) {
                self.pos += 1;
            }
            match self.s.get(self.pos) {
                Some(b'0'..=b'9') => {}
                None => return Err(self.eof_err("Unexpected end-of-input in a Number value".to_string())),
                Some(_) => return Err(self.number_char_err("Exponent indicator not followed by a digit")),
            }
            while let Some(b'0'..=b'9') = self.s.get(self.pos) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.pos]).unwrap_or("0");
        if text.len() > MAX_NUMBER_LEN {
            return Err(self.err_here(format!(
                "Number value length ({}) exceeds the maximum allowed ({MAX_NUMBER_LEN}, from `StreamReadConstraints.getMaxNumberLength()`)",
                text.len()
            )));
        }
        if float {
            return Ok(Value::Num(Number::Float(text.parse::<f64>().unwrap_or(f64::NAN))));
        }
        let digits = int_end - int_start;
        if digits <= 18 {
            return Ok(Value::Num(Number::Int(text.parse::<i64>().unwrap_or(0))));
        }
        if let Ok(i) = text.parse::<i64>() {
            return Ok(Value::Num(Number::Int(i)));
        }
        if !neg && let Ok(u) = text.parse::<u64>() {
            return Ok(Value::Num(Number::UInt(u)));
        }
        Ok(Value::Num(Number::Big(text.into())))
    }

    fn number_char_err(&mut self, what: &str) -> JsonError {
        let at = self.pos;
        let d = self.char_desc_at(at);
        self.pos += 1;
        self.err_prev_at(at, format!("Unexpected character ({d}) in numeric value: {what}"))
    }

    /// Parses a string body; the opening quote has been consumed.
    fn string(&mut self, name: bool) -> Result<String, JsonError> {
        let start = self.pos;
        // Fast path: no escapes.
        let mut i = self.pos;
        while i < self.s.len() {
            let b = self.s[i];
            if b == b'"' {
                if let Ok(s) = std::str::from_utf8(&self.s[start..i]) {
                    self.pos = i + 1;
                    return Ok(s.to_string());
                }
                break;
            }
            if b == b'\\' || !(0x20..0x80).contains(&b) {
                break;
            }
            i += 1;
        }
        let mut out: Vec<u8> = Vec::with_capacity(i - start + 16);
        out.extend_from_slice(&self.s[start..i]);
        self.pos = i;
        loop {
            if self.pos >= self.s.len() {
                let what = if name { "Unexpected end-of-input in field name" } else { "Unexpected end-of-input in VALUE_STRING" };
                return Err(self.eof_err(what.to_string()));
            }
            let b = self.s[self.pos];
            match b {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                0..=0x1f => {
                    self.pos += 1;
                    let target = if name { "name" } else { "string value" };
                    return Err(self.err_prev(format!(
                        "Illegal unquoted character ((CTRL-CHAR, code {b})): has to be escaped using backslash to be included in {target}"
                    )));
                }
                0x80.. => {
                    // Jackson reports a bad byte at its read position, which is just past the
                    // byte in values and past the closing quote in names (read whole first).
                    let len = utf8_len(b);
                    if len == 0 {
                        self.pos += 1;
                        if name {
                            self.skip_name_rest();
                        }
                        return Err(self.err_here(format!("Invalid UTF-8 start byte 0x{b:x}")));
                    }
                    if self.pos + len > self.s.len() {
                        self.pos = self.s.len();
                        let what = if name { "Unexpected end-of-input in field name" } else { "Unexpected end-of-input in VALUE_STRING" };
                        return Err(self.eof_err(what.to_string()));
                    }
                    let chunk = &self.s[self.pos..self.pos + len];
                    if let Some(i) = chunk[1..].iter().position(|c| *c & 0xc0 != 0x80) {
                        let bad = chunk[1 + i];
                        self.pos += i + 2;
                        if name {
                            self.skip_name_rest();
                        }
                        return Err(self.err_here(format!("Invalid UTF-8 middle byte 0x{bad:x}")));
                    }
                    // Well-formed continuation bytes are accepted as Jackson does (overlong forms
                    // and encoded surrogates become U+FFFD below).
                    out.extend_from_slice(chunk);
                    self.pos += len;
                }
                _ => {
                    out.push(b);
                    self.pos += 1;
                }
            }
        }
        // Escapes produce valid UTF-8 (lone surrogates become U+FFFD) and raw bytes were checked.
        Ok(String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
    }

    /// Moves past the rest of a field name and its closing quote.
    fn skip_name_rest(&mut self) {
        while self.pos < self.s.len() {
            match self.s[self.pos] {
                b'"' => {
                    self.pos += 1;
                    return;
                }
                b'\\' => self.pos += 2,
                _ => self.pos += 1,
            }
        }
        self.pos = self.s.len();
    }

    fn escape(&mut self, out: &mut Vec<u8>) -> Result<(), JsonError> {
        let Some(&c) = self.s.get(self.pos) else {
            return Err(self.eof_err("Unexpected end-of-input in character escape sequence".to_string()));
        };
        self.pos += 1;
        let ch = match c {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let hi = self.hex4()?;
                if (0xD800..0xDC00).contains(&hi) && self.s.get(self.pos) == Some(&b'\\') && self.s.get(self.pos + 1) == Some(&b'u') {
                    let save = self.pos;
                    self.pos += 2;
                    let lo = self.hex4()?;
                    if (0xDC00..0xE000).contains(&lo) {
                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                        char::from_u32(cp).unwrap_or('\u{FFFD}')
                    } else {
                        self.pos = save;
                        '\u{FFFD}'
                    }
                } else {
                    char::from_u32(hi).unwrap_or('\u{FFFD}')
                }
            }
            _ => {
                let d = self.char_desc_at(self.pos - 1);
                self.pos += utf8_len(c).max(1) - 1;
                return Err(self.err_prev_at(self.pos - utf8_len(c).max(1), format!("Unrecognized character escape {d}")));
            }
        };
        let mut buf = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let Some(&c) = self.s.get(self.pos) else {
                return Err(self.eof_err("Unexpected end-of-input in character escape sequence".to_string()));
            };
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => {
                    let at = self.pos;
                    let desc = self.char_desc_at(at);
                    self.pos += 1;
                    return Err(self.err_prev_at(at, format!("Unexpected character ({desc}): expected a hex-digit for character escape sequence")));
                }
            };
            v = v * 16 + d as u32;
            self.pos += 1;
        }
        Ok(v)
    }

    fn enter(&mut self) -> Result<(), JsonError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err_here(format!(
                "Document nesting depth ({}) exceeds the maximum allowed ({MAX_DEPTH}, from `StreamReadConstraints.getMaxNestingDepth()`)",
                self.depth
            )));
        }
        Ok(())
    }

    fn close_marker_err(&self, got: char, open: (usize, usize), in_array: bool) -> JsonError {
        let (want, what) = if in_array { (']', "Array") } else { ('}', "Object") };
        self.err_prev(format!(
            "Unexpected close marker '{got}': expected '{want}' (for {what} starting at {})",
            self.loc_desc(open.0, open.1)
        ))
    }

    /// Parses `"name"` `:` and the whitespace before the value; `pos` is at the name's quote.
    fn field_name(&mut self, map: &Map, open: (usize, usize)) -> Result<String, JsonError> {
        match self.s[self.pos] {
            b'"' => self.pos += 1,
            b']' => {
                self.pos += 1;
                return Err(self.close_marker_err(']', open, false));
            }
            _ => return Err(self.unexpected("was expecting double-quote to start field name")),
        }
        let key = self.string(true)?;
        if map.contains_key(&key) {
            return Err(self.err_here(format!("Duplicate field '{key}'")));
        }
        self.skip_ws_or_eof()?;
        if self.pos >= self.s.len() {
            return Err(self.eof_err("Unexpected end-of-input within/between Object entries".to_string()));
        }
        if self.s[self.pos] != b':' {
            return Err(self.unexpected("was expecting a colon to separate field name and value"));
        }
        self.pos += 1;
        self.skip_ws_or_eof()?;
        if self.pos >= self.s.len() {
            return Err(self.eof_err("Unexpected end-of-input within/between Object entries".to_string()));
        }
        Ok(key)
    }

    /// Parses one value with an explicit stack, so hostile nesting can't overflow the thread
    /// stack. `pos` is at the first byte of the value.
    fn root(&mut self) -> Result<Value, JsonError> {
        let mut stack: Vec<Frame> = Vec::new();
        'value: loop {
            let ctx = match stack.last() {
                None => Ctx::Root,
                Some(Frame::Obj { .. }) => Ctx::Object,
                Some(Frame::Arr { .. }) => Ctx::Array,
            };
            let mut v = match self.s[self.pos] {
                b'{' => {
                    let open = (self.line, self.pos - self.row_start + 1);
                    self.pos += 1;
                    self.enter()?;
                    self.skip_ws_or_eof()?;
                    if self.pos >= self.s.len() {
                        return Err(self.eof_in_ctx(Ctx::Object, open));
                    }
                    if self.s[self.pos] == b'}' {
                        self.pos += 1;
                        self.depth -= 1;
                        Value::Obj(Map::new())
                    } else {
                        let map = Map::new();
                        let key = self.field_name(&map, open)?;
                        stack.push(Frame::Obj { map, key, open });
                        continue 'value;
                    }
                }
                b'[' => {
                    let open = (self.line, self.pos - self.row_start + 1);
                    self.pos += 1;
                    self.enter()?;
                    self.skip_ws_or_eof()?;
                    if self.pos >= self.s.len() {
                        return Err(self.eof_in_ctx(Ctx::Array, open));
                    }
                    match self.s[self.pos] {
                        b']' => {
                            self.pos += 1;
                            self.depth -= 1;
                            Value::Arr(Vec::new())
                        }
                        b'}' => {
                            self.pos += 1;
                            return Err(self.close_marker_err('}', open, true));
                        }
                        _ => {
                            stack.push(Frame::Arr { items: Vec::new(), open });
                            continue 'value;
                        }
                    }
                }
                _ => self.value(ctx)?,
            };
            // Hand the finished value to its container, closing containers as they end.
            loop {
                match stack.last_mut() {
                    None => return Ok(v),
                    Some(Frame::Obj { map, key, open }) => {
                        let open = *open;
                        map.insert(std::mem::take(key), v);
                        self.skip_ws_or_eof()?;
                        if self.pos >= self.s.len() {
                            return Err(self.eof_in_ctx(Ctx::Object, open));
                        }
                        match self.s[self.pos] {
                            b'}' => {
                                self.pos += 1;
                                self.depth -= 1;
                                let Some(Frame::Obj { map, .. }) = stack.pop() else { unreachable!() };
                                v = Value::Obj(map);
                            }
                            b',' => {
                                self.pos += 1;
                                self.skip_ws_or_eof()?;
                                if self.pos >= self.s.len() {
                                    return Err(self.err_here("Unexpected end-of-input within/between Object entries".to_string()));
                                }
                                let Some(Frame::Obj { map, .. }) = stack.pop() else { unreachable!() };
                                let key = self.field_name(&map, open)?;
                                stack.push(Frame::Obj { map, key, open });
                                continue 'value;
                            }
                            b']' => {
                                self.pos += 1;
                                return Err(self.close_marker_err(']', open, false));
                            }
                            _ => return Err(self.unexpected("was expecting comma to separate Object entries")),
                        }
                    }
                    Some(Frame::Arr { items, open }) => {
                        let open = *open;
                        items.push(v);
                        self.skip_ws_or_eof()?;
                        if self.pos >= self.s.len() {
                            return Err(self.eof_in_ctx(Ctx::Array, open));
                        }
                        match self.s[self.pos] {
                            b']' => {
                                self.pos += 1;
                                self.depth -= 1;
                                let Some(Frame::Arr { items, .. }) = stack.pop() else { unreachable!() };
                                v = Value::Arr(items);
                            }
                            b',' => {
                                self.pos += 1;
                                self.skip_ws_or_eof()?;
                                if self.pos >= self.s.len() {
                                    return Err(self.err_here("Unexpected end-of-input within/between Array entries".to_string()));
                                }
                                continue 'value;
                            }
                            b'}' => {
                                self.pos += 1;
                                return Err(self.close_marker_err('}', open, true));
                            }
                            _ => return Err(self.unexpected("was expecting comma to separate Array entries")),
                        }
                    }
                }
            }
        }
    }
}

enum Frame {
    Obj { map: Map, key: String, open: (usize, usize) },
    Arr { items: Vec<Value>, open: (usize, usize) },
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        // Jackson reads 0xC0 and 0xC1 as (overlong) two-byte starts.
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 0,
    }
}

/// Decodes the code point at the start of `s` (the byte value for invalid sequences).
fn decode_char(s: &[u8]) -> u32 {
    if s.is_empty() {
        return 0;
    }
    let len = utf8_len(s[0]);
    if len <= 1 || s.len() < len {
        return s[0] as u32;
    }
    match std::str::from_utf8(&s[..len]) {
        Ok(t) => t.chars().next().map(|c| c as u32).unwrap_or(s[0] as u32),
        Err(_) => s[0] as u32,
    }
}

/// Jackson's `ParserMinimalBase._getCharDesc`.
pub(crate) fn char_desc(cp: u32) -> String {
    let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
    let iso_control = cp < 0x20 || (0x7f..=0x9f).contains(&cp);
    if iso_control {
        format!("(CTRL-CHAR, code {cp})")
    } else if cp > 255 {
        format!("'{ch}' (code {cp} / 0x{cp:x})")
    } else {
        format!("'{ch}' (code {cp})")
    }
}

fn is_java_identifier_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_java_identifier_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(s: &str) -> JsonError {
        parse(s.as_bytes()).unwrap_err()
    }

    #[test]
    fn values() {
        let input = format!(r#" {{"a":1,"b":[true,false,null],"c":"x{bs}u00e9{bs}n","d":-1.5e3,"e":{{}}}} trailing"#, bs = '\\');
        let v = parse(input.as_bytes()).unwrap().unwrap();
        assert_eq!(v.to_json(), "{\"a\":1,\"b\":[true,false,null],\"c\":\"xé\\n\",\"d\":-1500.0,\"e\":{}}");
        assert_eq!(parse(b"   ").unwrap(), None);
        assert_eq!(parse(b"/* c */ 1 // x").unwrap(), Some(Value::from(1)));
    }

    #[test]
    fn numbers() {
        let n = |s: &str| parse(s.as_bytes()).unwrap().unwrap();
        assert_eq!(n("9223372036854775807"), Value::Num(Number::Int(i64::MAX)));
        assert_eq!(n("-9223372036854775808"), Value::Num(Number::Int(i64::MIN)));
        assert_eq!(n("18446744073709551615"), Value::Num(Number::UInt(u64::MAX)));
        assert_eq!(n("123456789012345678901234567890"), Value::Num(Number::Big("123456789012345678901234567890".into())));
        assert_eq!(n("0.1"), Value::Num(Number::Float(0.1)));
        assert_eq!(n("-0"), Value::Num(Number::Int(0)));
    }

    #[test]
    fn surrogates() {
        let input = format!(r#""{bs}ud83d{bs}ude00""#, bs = '\\');
        assert_eq!(parse(input.as_bytes()).unwrap().unwrap(), Value::from("\u{1F600}"));
        assert_eq!(parse(br#""\uD800""#).unwrap().unwrap(), Value::from("\u{FFFD}"));
    }

    #[test]
    fn jackson_messages() {
        let e = err("{bad");
        assert_eq!((e.line, e.column, e.eof), (1, 2, false));
        assert_eq!(e.message, "Unexpected character ('b' (code 98)): was expecting double-quote to start field name");
        assert_eq!(e.location, "\n at [Source: (byte[])\"{bad\"; line: 1, column: 2]");
        let e = err(r#"{"a":1,"a":2}"#);
        assert_eq!((e.column, e.message.as_str()), (11, "Duplicate field 'a'"));
        let e = err("{");
        assert!(e.eof);
        assert_eq!(e.column, 2);
        assert_eq!(
            e.message,
            "Unexpected end-of-input: expected close marker for Object (start marker at [Source: (byte[])\"{\"; line: 1, column: 1])"
        );
        let e = err(r#"{"a":1,"#);
        assert!(!e.eof);
        assert_eq!(e.column, 8);
    }

    #[test]
    fn invalid_utf8_locations_match_jackson() {
        // Captured from Elasticsearch 9.5.5 (`PUT /idx` with these bodies).
        let e = |b: &[u8]| parse(b).unwrap_err();
        let x = e(b"{\"mappings\":{\"_meta\":{\"x\":\"ab\xffcd\"}}}");
        assert_eq!((x.message.as_str(), x.line, x.column), ("Invalid UTF-8 start byte 0xff", 1, 31));
        let x = e(b"{\"a\":\n \"\xc3\x28\"}");
        assert_eq!((x.message.as_str(), x.line, x.column), ("Invalid UTF-8 middle byte 0x28", 2, 5));
        let x = e(b"{\"mappings\":{\"_meta\":{\"x\xfe\":1}}}");
        assert_eq!((x.message.as_str(), x.line, x.column), ("Invalid UTF-8 start byte 0xfe", 1, 27));
        let x = e(b"{\"mappings\":{\"_meta\":{\"x\":\"\xe2\x82\"}}}");
        assert_eq!((x.message.as_str(), x.line, x.column), ("Invalid UTF-8 middle byte 0x22", 1, 31));
        assert!(x.location.contains("(byte[])\"{\"mappings\":{\"_meta\":{\"x\":\"\u{FFFD}\"}}}\""), "{}", x.location);
    }

    #[test]
    fn depth_limit() {
        let s = "[".repeat(1001);
        assert!(err(&s).message.starts_with("Document nesting depth (1001) exceeds the maximum allowed (1000"));
    }

    #[test]
    fn truncated_source() {
        let s = format!("{{\"a\":\"{}\"", "x".repeat(600));
        let e = err(&s);
        assert!(e.location.ends_with("\"[truncated 107 bytes]; line: 1, column: 608]"), "{}", e.location);
    }
}
