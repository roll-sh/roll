//! JSON reading as Elasticsearch's `JsonXContentParser` does it over Jackson's
//! `ReaderBasedJsonParser` (string sources, comments allowed, strict duplicate detection).
//!
//! Scripts see Jackson's behaviour wherever they parse JSON (`Json.load`, the ingest
//! `Processors.json`): which documents are accepted, the values they become (`HashMap`,
//! `ArrayList`, `Integer`/`Long`/`BigInteger`/`Double`) and, above all, the exact errors —
//! Jackson's messages with their `[Source: (String)"…"; line: L, column: C]` locations, wrapped
//! the way Elasticsearch wraps them (`x_content_parse_exception` / `x_content_e_o_f_exception`
//! caused by `json_parse_exception` / `json_e_o_f_exception`), with the stack frames of the
//! `XContentParser` call that failed.
//!
//! The tokenizer mirrors Jackson's state machine: a field name is read together with the first
//! token of its value (so a bad value fails in `nextFieldName`), strings are decoded lazily (a
//! bad string fails in `text()`), numbers are scanned eagerly, and every error reports the
//! location Jackson reports (the current position, the position of the offending character, or
//! the start of an unrecognized token).
//!
//! Plain documents (no comments, duplicates, big integers or odd escapes) first take the
//! byte-level reader in [`fast`], which yields the same values about three times faster.

use std::collections::HashSet;

use crate::types::ClassId;
use crate::value::{JList, JMap, Value};
use crate::vm::{Throw, Thrown};

mod fast;

const LF: u16 = b'\n' as u16;
const CR: u16 = b'\r' as u16;
const TAB: u16 = b'\t' as u16;
const SPACE: u16 = b' ' as u16;
const QUOTE: u16 = b'"' as u16;
const SLASH: u16 = b'/' as u16;
const BACKSLASH: u16 = b'\\' as u16;

/// Jackson's error report limits (`ErrorReportConfiguration` defaults).
const MAX_ERROR_TOKEN_LENGTH: usize = 256;
const MAX_RAW_CONTENT_LENGTH: usize = 500;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tok {
    StartObject,
    EndObject,
    StartArray,
    EndArray,
    FieldName,
    Str,
    Int,
    Float,
    True,
    False,
    Null,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Root,
    Array,
    Object,
}

struct Ctx {
    kind: Kind,
    index: i32,
    line: usize,
    col: usize,
    names: HashSet<String>,
}

impl Ctx {
    fn type_desc(&self) -> &'static str {
        match self.kind {
            Kind::Root => "root",
            Kind::Array => "Array",
            Kind::Object => "Object",
        }
    }
}

/// A Jackson exception: `JsonEOFException` (`eof`) or `JsonParseException`, with the message
/// before its location and the location.
#[derive(Clone, Debug)]
pub struct JacksonError {
    pub eof: bool,
    pub message: String,
    pub line: usize,
    pub column: usize,
    /// `[Source: …; line: L, column: C]`.
    pub location: String,
}

impl JacksonError {
    /// `JsonProcessingException.getMessage()`.
    pub fn jackson_message(&self) -> String {
        format!("{}\n at {}", self.message, self.location)
    }
    /// The Elasticsearch exception wrapping it: `(type, reason)`.
    pub fn x_content(&self) -> (&'static str, String) {
        if self.eof {
            ("x_content_e_o_f_exception", format!("[{}:{}] Unexpected end of file", self.line, self.column))
        } else {
            ("x_content_parse_exception", format!("[{}:{}] {}", self.line, self.column, self.jackson_message()))
        }
    }
}

/// Which `JsonXContentParser` method failed (it decides the innermost frames).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Call {
    NextToken,
    NextFieldName,
    Text,
}

/// An error of an `XContentParser` call, with the `AbstractXContentParser` frames above it.
#[derive(Debug)]
pub(crate) struct ReadError {
    pub err: JacksonError,
    pub call: Call,
    /// Innermost first.
    pub frames: Vec<String>,
}

const XCONTENT_IMPL: &str = "org.elasticsearch.xcontent.impl@9.5.5/org.elasticsearch.xcontent.provider.json.JsonXContentParser.";
const XCONTENT_SUPPORT: &str = "org.elasticsearch.xcontent@9.5.5/org.elasticsearch.xcontent.support.AbstractXContentParser.";

impl ReadError {
    /// The exception a script sees: Elasticsearch's x-content exception caused by Jackson's,
    /// with the parser frames then `outer` (the caller's frames, innermost first).
    pub fn thrown(&self, outer: &[&str]) -> Thrown {
        let (ty, reason) = self.err.x_content();
        let mut t = Throw::new(crate::types::cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(reason));
        t.es_type = Some(ty);
        if !self.err.eof {
            t.frames.push(format!("{XCONTENT_IMPL}newXContentParseException(JsonXContentParser.java:67)"));
        }
        t.frames.push(format!("{XCONTENT_IMPL}handleParserException(JsonXContentParser.java:{})", if self.err.eof { 76 } else { 77 }));
        t.frames.push(match self.call {
            Call::NextToken => format!("{XCONTENT_IMPL}nextToken(JsonXContentParser.java:92)"),
            Call::NextFieldName => format!("{XCONTENT_IMPL}nextFieldName(JsonXContentParser.java:101)"),
            Call::Text => format!("{XCONTENT_IMPL}text(JsonXContentParser.java:142)"),
        });
        t.frames.extend(self.frames.iter().cloned());
        t.frames.extend(outer.iter().map(|s| s.to_string()));
        let mut cause = Throw::new(crate::types::cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(self.err.jackson_message()));
        cause.es_type = Some(if self.err.eof { "json_e_o_f_exception" } else { "json_parse_exception" });
        t.with_cause(&cause)
    }
}

type R<T> = Result<T, JacksonError>;

/// Jackson's tokenizer over a whole string.
struct Lexer {
    buf: Vec<u16>,
    ptr: usize,
    row: usize,
    row_start: usize,
    ctx: Vec<Ctx>,
    curr: Option<Tok>,
    next: Option<Tok>,
    incomplete: bool,
    token_row: usize,
    token_col: usize,
    /// The current name, string or number text.
    text: Vec<u16>,
    int_len: usize,
    dups: bool,
}

fn char_desc(ch: u16) -> String {
    let c = ch as u32;
    let is_control = c <= 0x1F || (0x7F..=0x9F).contains(&c);
    if is_control {
        return format!("(CTRL-CHAR, code {c})");
    }
    let s = String::from_utf16_lossy(&[ch]);
    if c > 255 { format!("'{s}' (code {c} / 0x{c:x})") } else { format!("'{s}' (code {c})") }
}

fn is_digit(c: u16) -> bool {
    (b'0' as u16..=b'9' as u16).contains(&c)
}

impl Lexer {
    fn new(src: &str, dups: bool) -> Lexer {
        Lexer {
            buf: src.encode_utf16().collect(),
            ptr: 0,
            row: 1,
            row_start: 0,
            ctx: vec![Ctx { kind: Kind::Root, index: -1, line: 0, col: 0, names: HashSet::new() }],
            curr: None,
            next: None,
            incomplete: false,
            token_row: 1,
            token_col: 0,
            text: Vec::new(),
            int_len: 0,
            dups,
        }
    }

    fn end(&self) -> usize {
        self.buf.len()
    }

    // ---------------------------------------------------------------- locations and errors

    fn source_desc(&self) -> String {
        let mut out = String::from("(String)\"");
        let n = self.buf.len().min(MAX_RAW_CONTENT_LENGTH);
        let shown = String::from_utf16_lossy(&self.buf[..n]);
        for ch in shown.chars() {
            let c = ch as u32;
            if (c <= 0x1F || (0x7F..=0x9F).contains(&c)) && ch != '\r' && ch != '\n' {
                out.push_str(&format!("\\u{c:04X}"));
            } else {
                out.push(ch);
            }
        }
        out.push('"');
        if self.buf.len() > MAX_RAW_CONTENT_LENGTH {
            out.push_str(&format!("[truncated {} chars]", self.buf.len() - MAX_RAW_CONTENT_LENGTH));
        }
        out
    }

    fn location(&self, line: usize, col: usize) -> String {
        format!("[Source: {}; line: {line}, column: {col}]", self.source_desc())
    }

    fn error_at(&self, eof: bool, message: String, line: usize, column: usize) -> JacksonError {
        JacksonError { eof, message, line, column, location: self.location(line, column) }
    }

    /// At the current position (`currentLocation()`).
    fn err(&self, message: String) -> JacksonError {
        self.error_at(false, message, self.row, self.ptr - self.row_start + 1)
    }

    /// At the character just read (`_currentLocationMinusOne()`).
    fn err_prev(&self, message: String) -> JacksonError {
        self.error_at(false, message, self.row, self.ptr.saturating_sub(1).max(self.row_start) - self.row_start + 1)
    }

    fn eof(&self, what: &str) -> JacksonError {
        self.error_at(true, format!("Unexpected end-of-input{what}"), self.row, self.ptr - self.row_start + 1)
    }

    fn unexpected_char(&self, ch: u16, comment: &str) -> JacksonError {
        self.err_prev(format!("Unexpected character ({}): {comment}", char_desc(ch)))
    }

    fn unexpected_number_char(&self, ch: u16, comment: &str) -> JacksonError {
        self.err_prev(format!("Unexpected character ({}) in numeric value: {comment}", char_desc(ch)))
    }

    fn invalid_space(&self, ch: u16) -> JacksonError {
        self.err(format!("Illegal character ({}): only regular white space (\\r, \\n, \\t) is allowed between tokens", char_desc(ch)))
    }

    fn unquoted_space(&self, ch: u16, what: &str) -> JacksonError {
        self.err_prev(format!("Illegal unquoted character ({}): has to be escaped using backslash to be included in {what}", char_desc(ch)))
    }

    /// `_reportInvalidToken`: the token from where `matched` started, extended over identifier
    /// characters, reported at its start.
    fn invalid_token(&mut self, matched: &str) -> JacksonError {
        let start = self.ptr - matched.encode_utf16().count();
        let col = start - self.row_start + 1;
        let mut token: Vec<u16> = matched.encode_utf16().collect();
        while self.ptr < self.end() {
            let c = self.buf[self.ptr];
            if !crate::unicode::java_identifier_part(c as i32) {
                break;
            }
            self.ptr += 1;
            token.push(c);
            if token.len() >= MAX_ERROR_TOKEN_LENGTH {
                token.extend("...".encode_utf16());
                break;
            }
        }
        let msg = format!(
            "Unrecognized token '{}': was expecting (JSON String, Number, Array, Object or token 'null', 'true' or 'false')",
            String::from_utf16_lossy(&token)
        );
        self.error_at(false, msg, self.row, col)
    }

    fn top(&self) -> &Ctx {
        self.ctx.last().expect("root context")
    }

    fn start_location(&self) -> String {
        let c = self.top();
        self.location(c.line, c.col)
    }

    // ---------------------------------------------------------------- white space and comments

    fn skip_cr(&mut self) {
        if self.ptr < self.end() && self.buf[self.ptr] == LF {
            self.ptr += 1;
        }
        self.row += 1;
        self.row_start = self.ptr;
    }

    fn newline_or_space(&mut self, i: u16) -> R<()> {
        if i == LF {
            self.row += 1;
            self.row_start = self.ptr;
        } else if i == CR {
            self.skip_cr();
        } else if i != TAB && i != SPACE {
            return Err(self.invalid_space(i));
        }
        Ok(())
    }

    fn skip_comment(&mut self) -> R<()> {
        if self.ptr >= self.end() {
            return Err(self.eof(" in a comment"));
        }
        let c = self.buf[self.ptr];
        self.ptr += 1;
        if c == SLASH {
            while self.ptr < self.end() {
                let i = self.buf[self.ptr];
                self.ptr += 1;
                if i < SPACE {
                    if i == LF {
                        self.row += 1;
                        self.row_start = self.ptr;
                        break;
                    } else if i == CR {
                        self.skip_cr();
                        break;
                    } else if i != TAB {
                        return Err(self.invalid_space(i));
                    }
                }
            }
            Ok(())
        } else if c == b'*' as u16 {
            while self.ptr < self.end() {
                let i = self.buf[self.ptr];
                self.ptr += 1;
                if i == b'*' as u16 {
                    if self.ptr >= self.end() {
                        break;
                    }
                    if self.buf[self.ptr] == SLASH {
                        self.ptr += 1;
                        return Ok(());
                    }
                } else if i < SPACE {
                    self.newline_or_space(i)?;
                }
            }
            Err(self.eof(" in a comment"))
        } else {
            Err(self.unexpected_char(c, "was expecting either '*' or '/' for a comment"))
        }
    }

    /// The next significant character, or `None` at the end of the input (an error when a
    /// container is still open).
    fn skip_ws_or_end(&mut self) -> R<Option<u16>> {
        loop {
            if self.ptr >= self.end() {
                if self.top().kind != Kind::Root {
                    let marker = if self.top().kind == Kind::Array { "Array" } else { "Object" };
                    return Err(self.eof(&format!(": expected close marker for {marker} (start marker at {})", self.start_location())));
                }
                return Ok(None);
            }
            let i = self.buf[self.ptr];
            self.ptr += 1;
            if i > SPACE {
                if i == SLASH {
                    self.skip_comment()?;
                    continue;
                }
                return Ok(Some(i));
            }
            self.newline_or_space(i)?;
        }
    }

    fn skip_colon(&mut self) -> R<u16> {
        let mut got_colon = false;
        while self.ptr < self.end() {
            let i = self.buf[self.ptr];
            self.ptr += 1;
            if i > SPACE {
                if i == SLASH {
                    self.skip_comment()?;
                    continue;
                }
                if got_colon {
                    return Ok(i);
                }
                if i != b':' as u16 {
                    return Err(self.unexpected_char(i, "was expecting a colon to separate field name and value"));
                }
                got_colon = true;
                continue;
            }
            if i < SPACE {
                self.newline_or_space(i)?;
            }
        }
        Err(self.eof(&format!(" within/between {} entries", self.top().type_desc())))
    }

    fn skip_comma(&mut self, i: u16) -> R<u16> {
        if i != b',' as u16 {
            return Err(self.unexpected_char(i, &format!("was expecting comma to separate {} entries", self.top().type_desc())));
        }
        while self.ptr < self.end() {
            let i = self.buf[self.ptr];
            self.ptr += 1;
            if i > SPACE {
                if i == SLASH {
                    self.skip_comment()?;
                    continue;
                }
                return Ok(i);
            }
            if i < SPACE {
                self.newline_or_space(i)?;
            }
        }
        Err(self.err(format!("Unexpected end-of-input within/between {} entries", self.top().type_desc())))
    }

    // ---------------------------------------------------------------- contexts

    fn expect_comma(&mut self) -> bool {
        let c = self.ctx.last_mut().expect("root context");
        c.index += 1;
        c.kind != Kind::Root && c.index > 0
    }

    fn push_ctx(&mut self, kind: Kind) {
        let (line, col) = (self.token_row, self.token_col);
        self.ctx.push(Ctx { kind, index: -1, line, col, names: HashSet::new() });
    }

    fn update_location(&mut self) {
        self.token_row = self.row;
        self.token_col = self.ptr - self.row_start;
    }

    fn close_scope(&mut self, i: u16) -> R<()> {
        self.update_location();
        let (want, other) = if i == b']' as u16 { (Kind::Array, '}') } else { (Kind::Object, ']') };
        if self.top().kind != want {
            let act = i as u8 as char;
            if self.top().kind == Kind::Root {
                let scope = if act == '}' { "Object" } else { "Array" };
                return Err(self.err_prev(format!("Unexpected close marker '{act}': no open {scope} to close")));
            }
            let msg = format!("Unexpected close marker '{act}': expected '{other}' (for {} starting at {})", self.top().type_desc(), self.start_location());
            return Err(self.err_prev(msg));
        }
        self.ctx.pop();
        self.curr = Some(if want == Kind::Array { Tok::EndArray } else { Tok::EndObject });
        Ok(())
    }

    // ---------------------------------------------------------------- strings

    fn decode_escaped(&mut self) -> R<u16> {
        if self.ptr >= self.end() {
            return Err(self.eof(" in character escape sequence"));
        }
        let c = self.buf[self.ptr];
        self.ptr += 1;
        Ok(match c as u8 as char {
            _ if c > 0x7F => return Err(self.err_prev(format!("Unrecognized character escape {}", char_desc(c)))),
            'b' => 8,
            't' => TAB,
            'n' => LF,
            'f' => 12,
            'r' => CR,
            '"' | '/' | '\\' => c,
            'u' => {
                let mut v: u16 = 0;
                for _ in 0..4 {
                    if self.ptr >= self.end() {
                        return Err(self.eof(" in character escape sequence"));
                    }
                    let ch = self.buf[self.ptr];
                    self.ptr += 1;
                    let Some(d) = char::from_u32(ch as u32).and_then(|x| x.to_digit(16)).filter(|_| ch < 0x80) else {
                        return Err(self.unexpected_char(ch, "expected a hex-digit for character escape sequence"));
                    };
                    v = (v << 4) | d as u16;
                }
                v
            }
            _ => return Err(self.err_prev(format!("Unrecognized character escape {}", char_desc(c)))),
        })
    }

    /// A field name (the opening quote read).
    fn parse_name(&mut self) -> R<String> {
        let mut out = Vec::new();
        loop {
            if self.ptr >= self.end() {
                return Err(self.eof(" in field name"));
            }
            let mut c = self.buf[self.ptr];
            self.ptr += 1;
            if c == BACKSLASH {
                c = self.decode_escaped()?;
            } else if c == QUOTE {
                break;
            } else if c < SPACE {
                return Err(self.unquoted_space(c, "name"));
            }
            out.push(c);
        }
        Ok(String::from_utf16_lossy(&out))
    }

    /// Decodes the pending string value (`_finishString`).
    fn finish_string(&mut self) -> R<()> {
        self.incomplete = false;
        self.text.clear();
        loop {
            if self.ptr >= self.end() {
                return Err(self.eof(": was expecting closing quote for a string value"));
            }
            let mut c = self.buf[self.ptr];
            self.ptr += 1;
            if c == QUOTE {
                return Ok(());
            } else if c == BACKSLASH {
                c = self.decode_escaped()?;
            } else if c < SPACE {
                return Err(self.unquoted_space(c, "string value"));
            }
            self.text.push(c);
        }
    }

    // ---------------------------------------------------------------- literals

    /// `_matchToken(matchStr, i)`: the rest of a literal, which must not run into an identifier.
    fn match_token(&mut self, word: &str, mut i: usize) -> R<()> {
        let w: Vec<u16> = word.encode_utf16().collect();
        while i < w.len() {
            if self.ptr >= self.end() || self.buf[self.ptr] != w[i] {
                return Err(self.invalid_token(&word[..i]));
            }
            self.ptr += 1;
            i += 1;
        }
        if self.ptr >= self.end() {
            return Ok(());
        }
        let ch = self.buf[self.ptr];
        if ch >= b'0' as u16 && ch != b']' as u16 && ch != b'}' as u16 && crate::unicode::java_identifier_part(ch as i32) {
            return Err(self.invalid_token(&word[..i]));
        }
        Ok(())
    }

    fn odd_value(&mut self, i: u16) -> R<Tok> {
        match i as u8 as char {
            _ if i > 0x7F => {}
            'N' => {
                self.match_token("NaN", 1)?;
                return Err(self.err("Non-standard token 'NaN': enable `JsonReadFeature.ALLOW_NON_NUMERIC_NUMBERS` to allow".into()));
            }
            'I' => {
                self.match_token("Infinity", 1)?;
                return Err(self.err("Non-standard token 'Infinity': enable `JsonReadFeature.ALLOW_NON_NUMERIC_NUMBERS` to allow".into()));
            }
            '+' => {
                if self.ptr >= self.end() {
                    return Err(self.eof(" in a Number value"));
                }
                let c = self.buf[self.ptr];
                self.ptr += 1;
                return self.invalid_number_start(c, false, true);
            }
            _ => {}
        }
        if crate::unicode::java_identifier_start(i as i32) {
            return Err(self.invalid_token(&String::from_utf16_lossy(&[i])));
        }
        Err(self.unexpected_char(i, "expected a valid value (JSON String, Number, Array, Object or token 'null', 'true' or 'false')"))
    }

    // ---------------------------------------------------------------- numbers

    fn invalid_number_start(&mut self, mut ch: u16, negative: bool, has_sign: bool) -> R<Tok> {
        if ch == b'I' as u16 {
            if self.ptr >= self.end() {
                return Err(self.eof(" in a Number value"));
            }
            ch = self.buf[self.ptr];
            self.ptr += 1;
            let sign = if negative { "-" } else { "+" };
            if ch == b'N' as u16 || ch == b'n' as u16 {
                let word = if ch == b'N' as u16 { format!("{sign}INF") } else { format!("{sign}Infinity") };
                self.match_token(&word, 3)?;
                return Err(self.err(format!("Non-standard token '{word}': enable `JsonReadFeature.ALLOW_NON_NUMERIC_NUMBERS` to allow")));
            }
        }
        if has_sign && !negative {
            return Err(self.unexpected_number_char(
                b'+' as u16,
                "JSON spec does not allow numbers to have plus signs: enable `JsonReadFeature.ALLOW_LEADING_PLUS_SIGN_FOR_NUMBERS` to allow",
            ));
        }
        let msg = if negative { "expected digit (0-9) to follow minus sign, for valid numeric value" } else { "expected digit (0-9) for valid numeric value" };
        Err(self.unexpected_number_char(ch, msg))
    }

    fn verify_root_space(&mut self, ch: u16) -> R<()> {
        self.ptr += 1;
        match ch {
            SPACE | TAB => Ok(()),
            CR => {
                self.ptr -= 1;
                Ok(())
            }
            LF => {
                self.row += 1;
                self.row_start = self.ptr;
                Ok(())
            }
            _ => Err(self.unexpected_char(ch, "Expected space separating root-level values")),
        }
    }

    fn in_root(&self) -> bool {
        self.top().kind == Kind::Root
    }

    fn number_text(&mut self, start: usize, end: usize, int_len: usize, float: bool) -> Tok {
        self.text.clear();
        self.text.extend_from_slice(&self.buf[start..end]);
        self.int_len = int_len;
        if float { Tok::Float } else { Tok::Int }
    }

    /// `_parseUnsignedNumber` (the first digit read).
    fn parse_unsigned(&mut self, ch: u16) -> R<Tok> {
        let start = self.ptr - 1;
        if ch == b'0' as u16 {
            return self.parse_number2(false, start);
        }
        let mut ptr = self.ptr;
        let mut int_len = 1;
        let mut ch;
        loop {
            if ptr >= self.end() {
                self.ptr = start;
                return self.parse_number2(false, start);
            }
            ch = self.buf[ptr];
            ptr += 1;
            if !is_digit(ch) {
                break;
            }
            int_len += 1;
        }
        if ch == b'.' as u16 || (ch | 0x20) == b'e' as u16 {
            self.ptr = ptr;
            return self.parse_float(ch, start, ptr, false, int_len);
        }
        ptr -= 1;
        self.ptr = ptr;
        if self.in_root() {
            self.verify_root_space(ch)?;
        }
        Ok(self.number_text(start, ptr, int_len, false))
    }

    /// `_parseSignedNumber` (the sign read).
    fn parse_signed(&mut self, negative: bool) -> R<Tok> {
        let mut ptr = self.ptr;
        let start = if negative { ptr - 1 } else { ptr };
        if ptr >= self.end() {
            return self.parse_number2(negative, start);
        }
        let mut ch = self.buf[ptr];
        ptr += 1;
        if !is_digit(ch) {
            self.ptr = ptr;
            if ch == b'.' as u16 {
                return self.odd_value(b'.' as u16);
            }
            return self.invalid_number_start(ch, negative, true);
        }
        if ch == b'0' as u16 {
            return self.parse_number2(negative, start);
        }
        let mut int_len = 1;
        loop {
            if ptr >= self.end() {
                return self.parse_number2(negative, start);
            }
            ch = self.buf[ptr];
            ptr += 1;
            if !is_digit(ch) {
                break;
            }
            int_len += 1;
        }
        if ch == b'.' as u16 || (ch | 0x20) == b'e' as u16 {
            self.ptr = ptr;
            return self.parse_float(ch, start, ptr, negative, int_len);
        }
        ptr -= 1;
        self.ptr = ptr;
        if self.in_root() {
            self.verify_root_space(ch)?;
        }
        Ok(self.number_text(start, ptr, int_len, false))
    }

    fn parse_float(&mut self, mut ch: u16, start: usize, mut ptr: usize, neg: bool, int_len: usize) -> R<Tok> {
        let end = self.end();
        if ch == b'.' as u16 {
            let mut fract_len = 0;
            loop {
                if ptr >= end {
                    return self.parse_number2(neg, start);
                }
                ch = self.buf[ptr];
                ptr += 1;
                if !is_digit(ch) {
                    break;
                }
                fract_len += 1;
            }
            if fract_len == 0 {
                return Err(self.unexpected_number_char(ch, "Decimal point not followed by a digit"));
            }
        }
        if (ch | 0x20) == b'e' as u16 {
            if ptr >= end {
                self.ptr = start;
                return self.parse_number2(neg, start);
            }
            ch = self.buf[ptr];
            ptr += 1;
            if ch == b'-' as u16 || ch == b'+' as u16 {
                if ptr >= end {
                    self.ptr = start;
                    return self.parse_number2(neg, start);
                }
                ch = self.buf[ptr];
                ptr += 1;
            }
            let mut exp_len = 0;
            while is_digit(ch) {
                exp_len += 1;
                if ptr >= end {
                    self.ptr = start;
                    return self.parse_number2(neg, start);
                }
                ch = self.buf[ptr];
                ptr += 1;
            }
            if exp_len == 0 {
                return Err(self.unexpected_number_char(ch, "Exponent indicator not followed by a digit"));
            }
        }
        ptr -= 1;
        self.ptr = ptr;
        if self.in_root() {
            self.verify_root_space(ch)?;
        }
        Ok(self.number_text(start, ptr, int_len, true))
    }

    /// The careful number scanner (`_parseNumber2`): leading zeros and the end of the input.
    fn parse_number2(&mut self, neg: bool, start: usize) -> R<Tok> {
        self.ptr = if neg { start + 1 } else { start };
        let mut out: Vec<u16> = Vec::new();
        if neg {
            out.push(b'-' as u16);
        }
        let mut int_len = 0;
        let mut c = if self.ptr < self.end() {
            self.ptr += 1;
            self.buf[self.ptr - 1]
        } else {
            return Err(self.eof("No digit following sign"));
        };
        if c == b'0' as u16 {
            c = self.verify_no_leading_zeroes()?;
        }
        let mut eof = false;
        while is_digit(c) {
            int_len += 1;
            out.push(c);
            if self.ptr >= self.end() {
                c = 0;
                eof = true;
                break;
            }
            c = self.buf[self.ptr];
            self.ptr += 1;
        }
        if int_len == 0 {
            return self.invalid_number_start(c, neg, false);
        }
        let mut float = false;
        if c == b'.' as u16 {
            float = true;
            out.push(c);
            let mut fract_len = 0;
            loop {
                if self.ptr >= self.end() {
                    eof = true;
                    break;
                }
                c = self.buf[self.ptr];
                self.ptr += 1;
                if !is_digit(c) {
                    break;
                }
                fract_len += 1;
                out.push(c);
            }
            if fract_len == 0 {
                return Err(self.unexpected_number_char(c, "Decimal point not followed by a digit"));
            }
        }
        if (c | 0x20) == b'e' as u16 {
            float = true;
            out.push(c);
            let next = |me: &mut Self| -> R<u16> {
                if me.ptr < me.end() {
                    me.ptr += 1;
                    Ok(me.buf[me.ptr - 1])
                } else {
                    Err(me.eof("expected a digit for number exponent"))
                }
            };
            c = next(self)?;
            if c == b'-' as u16 || c == b'+' as u16 {
                out.push(c);
                c = next(self)?;
            }
            let mut exp_len = 0;
            while is_digit(c) {
                exp_len += 1;
                out.push(c);
                if self.ptr >= self.end() {
                    eof = true;
                    break;
                }
                c = self.buf[self.ptr];
                self.ptr += 1;
            }
            if exp_len == 0 {
                return Err(self.unexpected_number_char(c, "Exponent indicator not followed by a digit"));
            }
        }
        if !eof {
            self.ptr -= 1;
            if self.in_root() {
                self.verify_root_space(c)?;
            }
        }
        self.text = out;
        self.int_len = int_len;
        Ok(if float { Tok::Float } else { Tok::Int })
    }

    fn verify_no_leading_zeroes(&mut self) -> R<u16> {
        if self.ptr >= self.end() {
            return Ok(b'0' as u16);
        }
        if !is_digit(self.buf[self.ptr]) {
            return Ok(b'0' as u16);
        }
        Err(self.err("Invalid numeric value: Leading zeroes not allowed".into()))
    }

    // ---------------------------------------------------------------- values

    /// The value token starting with `i`: `in_object` defers the context of a container to
    /// `_nextAfterName`; `via_field_name` is `nextFieldName`'s switch, which has no case of its
    /// own for a stray `}`.
    fn value_token(&mut self, i: u16, in_object: bool, via_field_name: bool) -> R<Tok> {
        Ok(match i as u8 as char {
            _ if i > 0x7F => self.odd_value(i)?,
            '"' => {
                self.incomplete = true;
                Tok::Str
            }
            '[' => {
                if !in_object {
                    self.push_ctx(Kind::Array);
                }
                Tok::StartArray
            }
            '{' => {
                if !in_object {
                    self.push_ctx(Kind::Object);
                }
                Tok::StartObject
            }
            '}' if !via_field_name => return Err(self.unexpected_char(i, "expected a value")),
            't' => {
                self.match_token("true", 1)?;
                Tok::True
            }
            'f' => {
                self.match_token("false", 1)?;
                Tok::False
            }
            'n' => {
                self.match_token("null", 1)?;
                Tok::Null
            }
            '-' => self.parse_signed(true)?,
            '.' => self.odd_value(i)?,
            '0'..='9' => self.parse_unsigned(i)?,
            _ => self.odd_value(i)?,
        })
    }

    fn after_name(&mut self) -> Tok {
        let t = self.next.take().expect("value after a field name");
        if t == Tok::StartArray {
            self.push_ctx(Kind::Array);
        } else if t == Tok::StartObject {
            self.push_ctx(Kind::Object);
        }
        self.curr = Some(t);
        t
    }

    fn skip_pending_string(&mut self) -> R<()> {
        if self.incomplete {
            self.finish_string()?;
        }
        Ok(())
    }

    fn set_current_name(&mut self, name: &str) -> R<()> {
        if self.dups {
            let fresh = self.ctx.last_mut().expect("root context").names.insert(name.to_string());
            if !fresh {
                return Err(self.err(format!("Duplicate field '{name}'")));
            }
        }
        Ok(())
    }

    /// The field name (`nextToken`/`nextFieldName` in an object), then the first token of its
    /// value.
    fn field(&mut self, i: u16, via_field_name: bool) -> R<String> {
        let name = if i == QUOTE {
            self.parse_name()?
        } else {
            return Err(self.unexpected_char(i, "was expecting double-quote to start field name"));
        };
        self.set_current_name(&name)?;
        self.text = name.encode_utf16().collect();
        self.curr = Some(Tok::FieldName);
        let i = self.skip_colon()?;
        self.update_location();
        let t = self.value_token(i, true, via_field_name)?;
        self.next = Some(t);
        Ok(name)
    }

    fn next_token(&mut self) -> R<Option<Tok>> {
        if self.curr == Some(Tok::FieldName) {
            return Ok(Some(self.after_name()));
        }
        self.skip_pending_string()?;
        let Some(mut i) = self.skip_ws_or_end()? else {
            self.curr = None;
            return Ok(None);
        };
        if (i | 0x20) == b'}' as u16 {
            self.close_scope(i)?;
            return Ok(self.curr);
        }
        if self.expect_comma() {
            i = self.skip_comma(i)?;
        }
        if self.top().kind == Kind::Object {
            self.field(i, false)?;
            return Ok(Some(Tok::FieldName));
        }
        self.update_location();
        let t = self.value_token(i, false, false)?;
        self.curr = Some(t);
        Ok(Some(t))
    }

    fn next_field_name(&mut self) -> R<Option<String>> {
        if self.curr == Some(Tok::FieldName) {
            self.after_name();
            return Ok(None);
        }
        self.skip_pending_string()?;
        let Some(mut i) = self.skip_ws_or_end()? else {
            self.curr = None;
            return Ok(None);
        };
        if (i | 0x20) == b'}' as u16 {
            self.close_scope(i)?;
            return Ok(None);
        }
        if self.expect_comma() {
            i = self.skip_comma(i)?;
        }
        if self.top().kind != Kind::Object {
            self.update_location();
            let t = self.value_token(i, false, true)?;
            self.curr = Some(t);
            return Ok(None);
        }
        self.field(i, true).map(Some)
    }

    fn text(&mut self) -> R<String> {
        if self.curr == Some(Tok::Str) && self.incomplete {
            self.finish_string()?;
        }
        Ok(String::from_utf16_lossy(&self.text))
    }

    /// `getNumberValue()`: `Integer`, `Long` or `BigInteger` by length and range, or `Double`.
    fn number_value(&self) -> Value {
        let t = String::from_utf16_lossy(&self.text);
        if self.curr == Some(Tok::Float) {
            return Value::Double(t.parse().unwrap_or(f64::NAN));
        }
        if self.int_len <= 9 {
            return Value::Int(t.parse().unwrap_or(0));
        }
        if self.int_len <= 18 {
            let l: i64 = t.parse().unwrap_or(0);
            if self.int_len == 10 && i32::try_from(l).is_ok() {
                return Value::Int(l as i32);
            }
            return Value::Long(l);
        }
        match t.parse::<i64>() {
            Ok(l) => Value::Long(l),
            Err(_) => crate::bignum::big_integer_value(&t),
        }
    }
}

/// An `XContentParser` over a JSON string, reading values the way `AbstractXContentParser`
/// does and recording its frames for errors.
pub(crate) struct XContentReader {
    lx: Lexer,
    /// `(method, line)` of the `AbstractXContentParser` calls in progress, outermost first.
    stack: Vec<(&'static str, u32)>,
    map_class: ClassId,
}

impl XContentReader {
    pub fn new(src: &str, map_class: ClassId) -> XContentReader {
        XContentReader { lx: Lexer::new(src, true), stack: Vec::new(), map_class }
    }

    fn fail(&self, err: JacksonError, call: Call) -> ReadError {
        let frames = self.stack.iter().rev().map(|(m, l)| format!("{XCONTENT_SUPPORT}{m}(AbstractXContentParser.java:{l})")).collect();
        ReadError { err, call, frames }
    }

    fn at(&mut self, line: u32) {
        if let Some(top) = self.stack.last_mut() {
            top.1 = line;
        }
    }

    pub fn next_token(&mut self) -> Result<Option<Tok>, ReadError> {
        self.lx.next_token().map_err(|e| self.fail(e, Call::NextToken))
    }

    fn next_field_name(&mut self) -> Result<Option<String>, ReadError> {
        self.lx.next_field_name().map_err(|e| self.fail(e, Call::NextFieldName))
    }

    pub fn text(&mut self) -> Result<String, ReadError> {
        self.lx.text().map_err(|e| self.fail(e, Call::Text))
    }

    pub fn number_value(&self) -> Value {
        self.lx.number_value()
    }

    fn new_map(&self) -> std::rc::Rc<JMap> {
        std::rc::Rc::new(JMap::new(self.map_class))
    }

    /// `map()` on the current `START_OBJECT`.
    pub fn map(&mut self) -> Result<Value, ReadError> {
        self.stack.push(("map", 325));
        self.stack.push(("readMapSafe", 383));
        let m = self.new_map();
        self.stack.push(("findNonEmptyMapStart", 414));
        let first = self.next_field_name()?;
        self.stack.pop();
        if let Some(first) = first {
            self.at(384);
            self.read_map_entries(&m, first)?;
        }
        self.stack.pop();
        self.stack.pop();
        Ok(Value::Map(m))
    }

    /// `list()` on the current `START_ARRAY`.
    pub fn list(&mut self) -> Result<Value, ReadError> {
        self.stack.push(("list", 366));
        let l = self.read_list()?;
        self.stack.pop();
        Ok(l)
    }

    fn read_map_entries(&mut self, m: &std::rc::Rc<JMap>, first: String) -> Result<(), ReadError> {
        self.stack.push(("readMapEntries", 394));
        let mut name = first;
        loop {
            self.at(394);
            let t = self.next_token()?;
            let v = self.read_value(t)?;
            m.insert_raw(Value::string(name), v);
            self.at(396);
            match self.next_field_name()? {
                Some(n) => name = n,
                None => break,
            }
        }
        self.stack.pop();
        Ok(())
    }

    fn read_list(&mut self) -> Result<Value, ReadError> {
        self.stack.push(("readListUnsafe", 441));
        let mut items = Vec::new();
        let mut t = self.next_token()?;
        while let Some(tok) = t {
            if tok == Tok::EndArray {
                break;
            }
            self.at(442);
            items.push(self.read_value(Some(tok))?);
            self.at(441);
            t = self.next_token()?;
        }
        self.stack.pop();
        Ok(JList::array_list(items))
    }

    fn read_value(&mut self, t: Option<Tok>) -> Result<Value, ReadError> {
        self.stack.push(("readValueUnsafe", 464));
        let v = match t {
            Some(Tok::Str) => Value::string(self.text()?),
            Some(Tok::Int | Tok::Float) => self.number_value(),
            Some(Tok::True) => Value::Bool(true),
            Some(Tok::False) => Value::Bool(false),
            Some(Tok::StartObject) => {
                self.at(471);
                let m = self.new_map();
                if let Some(first) = self.next_field_name()? {
                    self.at(472);
                    self.read_map_entries(&m, first)?;
                }
                Value::Map(m)
            }
            Some(Tok::StartArray) => {
                self.at(475);
                self.read_list()?
            }
            _ => Value::Null,
        };
        self.stack.pop();
        Ok(v)
    }
}

/// The value of a whole document the way `Json.load` reads it: the first token decides
/// (objects, arrays, numbers, booleans, strings; anything else is null) and what follows is
/// ignored. `None` when the input holds no token at all.
pub(crate) fn load(src: &str, map_class: ClassId) -> Result<Option<Value>, ReadError> {
    // plain JSON (stored sources, most arguments) takes a byte-level pass; anything Jackson
    // treats specially or rejects goes through the exact reader below
    if let Some(v) = fast::load(src, map_class) {
        return Ok(Some(v));
    }
    let mut r = XContentReader::new(src, map_class);
    Ok(Some(match r.next_token()? {
        None => return Ok(None),
        Some(Tok::StartArray) => r.list()?,
        Some(Tok::StartObject) => r.map()?,
        Some(Tok::Int | Tok::Float) => r.number_value(),
        Some(Tok::True) => Value::Bool(true),
        Some(Tok::False) => Value::Bool(false),
        Some(Tok::Str) => Value::string(r.text()?),
        Some(_) => Value::Null,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::cls;

    /// The fast reader gives the exact reader's values, or hands over.
    #[test]
    fn fast_reader_agrees_with_the_exact_one() {
        let cases = [
            r#"{"a": [1, 2.5, "s", null, true, false], "b": {"c": -0, "d": 2147483647, "e": 2147483648, "f": -2147483648, "g": 999999999999999999, "h": 1234567890}}"#,
            r#"["\u00e9\t\"\\\/\b\f\n\r", "\ud83d\ude00", "plain é ☃ 😀", 1e3, -1.5E-7, 0.0, -0.0, 1e400, 1.7976931348623157e308, 4.9e-324]"#,
            "  {\"x\" :  { } , \"y\":[ ] ,\n\t\"z\": [[[]]]}  ",
            "[]",
            "12",
            "-7",
            "\"s\"",
            "true",
            "null",
        ];
        for src in cases {
            let fast = fast::load(src, cls::LINKED_HASH_MAP).unwrap_or_else(|| panic!("{src}"));
            let mut r = XContentReader::new(src, cls::LINKED_HASH_MAP);
            let exact = match r.next_token().unwrap() {
                Some(Tok::StartArray) => r.list().unwrap(),
                Some(Tok::StartObject) => r.map().unwrap(),
                Some(Tok::Int | Tok::Float) => r.number_value(),
                Some(Tok::Str) => Value::string(r.text().unwrap()),
                Some(Tok::True) => Value::Bool(true),
                _ => Value::Null,
            };
            assert_eq!(format!("{fast:?}"), format!("{exact:?}"), "{src}");
        }
        let handed_over = [
            "{\"a\":1,\"a\":2}",
            "[01]",
            "[1 // c\n]",
            "[1 /* c */]",
            "[1,]",
            "{\"a\" 1}",
            "{'a': 1}",
            "[\"\\ud800\"]",
            "[\"\\udc00\"]",
            "[\"\\x\"]",
            "[12345678901234567890]",
            "[1234567890123456789]",
            "tru",
            "truex",
            "[1] x",
            "",
            "   ",
            "[-]",
            "[1.]",
            "[.5]",
            "[1e]",
            "[1x]",
            "[NaN]",
            "[\"a\u{1}\"]",
            "[\"unterminated",
        ];
        for src in handed_over {
            assert!(fast::load(src, cls::HASH_MAP).is_none(), "{src}");
        }
        let deep = "[".repeat(300) + &"]".repeat(300);
        assert!(fast::load(&deep, cls::HASH_MAP).is_none());
        assert!(load(&deep, cls::HASH_MAP).unwrap().is_some());
    }

    /// Mutated documents: whenever the fast reader accepts one, the exact reader reads the same.
    #[test]
    fn fast_reader_agrees_on_mutations() {
        let seed = r#"{"a": [1, -2.5e3, "s\"é", null, true, false], "b": {"c": -0, "d": 2147483648, "e": "x\\y"}, "f": "😀"}"#;
        let pieces = ["{", "}", "[", "]", ",", ":", "\"", "\\", "0", "1", "-", ".", "e", "+", " ", "\n", "/", "*", "\\u", "d83d", "true", "null", "\u{1}", "é"];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D) as usize
        };
        let mut accepted = 0;
        for _ in 0..20_000 {
            let mut s: Vec<char> = seed.chars().collect();
            for _ in 0..1 + next() % 3 {
                let at = next() % (s.len() + 1);
                match next() % 3 {
                    0 if at < s.len() => {
                        s.remove(at);
                    }
                    1 if at < s.len() => s[at] = pieces[next() % pieces.len()].chars().next().unwrap_or(' '),
                    _ => s.splice(at..at, pieces[next() % pieces.len()].chars()).for_each(drop),
                }
            }
            let src: String = s.into_iter().collect();
            if let Some(fast) = fast::load(&src, cls::LINKED_HASH_MAP) {
                accepted += 1;
                let mut r = XContentReader::new(&src, cls::LINKED_HASH_MAP);
                let exact = match r.next_token() {
                    Ok(Some(Tok::StartArray)) => r.list(),
                    Ok(Some(Tok::StartObject)) => r.map(),
                    Ok(Some(Tok::Int | Tok::Float)) => Ok(r.number_value()),
                    Ok(Some(Tok::Str)) => r.text().map(Value::string),
                    Ok(Some(Tok::True)) => Ok(Value::Bool(true)),
                    Ok(Some(Tok::False)) => Ok(Value::Bool(false)),
                    Ok(_) => Ok(Value::Null),
                    Err(e) => Err(e),
                };
                let exact = exact.unwrap_or_else(|e| panic!("{src:?}: fast accepted, exact failed: {}", e.thrown(&[]).message.unwrap_or_default()));
                assert_eq!(format!("{fast:?}"), format!("{exact:?}"), "{src:?}");
                // and nothing follows that the exact reader would reject
                while let Ok(Some(_)) = r.next_token() {}
                assert!(r.next_token().is_ok(), "{src:?}");
            }
        }
        assert!(accepted > 1000, "{accepted}");
    }

    fn err(src: &str) -> String {
        match load(src, cls::HASH_MAP) {
            Err(e) => e.err.x_content().1,
            Ok(v) => format!("ok {:?}", v.map(|v| v.to_java_string())),
        }
    }

    #[test]
    fn values_and_errors() {
        assert_eq!(err("{\"a\": [1, 2.5, \"s\", null, true]}"), "ok Some(\"{a=[1, 2.5, s, null, true]}\")");
        assert_eq!(err("[1 2]"), "[1:4] Unexpected character ('2' (code 50)): was expecting comma to separate Array entries\n at [Source: (String)\"[1 2]\"; line: 1, column: 4]");
        assert_eq!(err("{\"a\":1,\"a\":2}"), "[1:11] Duplicate field 'a'\n at [Source: (String)\"{\"a\":1,\"a\":2}\"; line: 1, column: 11]");
        assert_eq!(err("[1"), "[1:3] Unexpected end of file");
        assert_eq!(err("tru"), "[1:1] Unrecognized token 'tru': was expecting (JSON String, Number, Array, Object or token 'null', 'true' or 'false')\n at [Source: (String)\"tru\"; line: 1, column: 1]");
        assert_eq!(err("01"), "[1:2] Invalid numeric value: Leading zeroes not allowed\n at [Source: (String)\"01\"; line: 1, column: 2]");
        assert_eq!(err("1 // c"), "ok Some(\"1\")");
        assert_eq!(err("2147483648"), "ok Some(\"2147483648\")");
    }
}
