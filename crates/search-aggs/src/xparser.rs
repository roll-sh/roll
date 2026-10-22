//! A pull parser over a [`Json`] value with Elasticsearch's `XContentParser` semantics, so the
//! aggregation parsers follow Elasticsearch's token-by-token logic and fail with the same
//! exception types, texts and `[line:col]` locations.
//!
//! The request body is parsed once (by `search-es`); aggregation parsing walks that tree as a
//! token stream (`START_OBJECT`, `FIELD_NAME`, `VALUE_STRING`, …). Locations are computed only
//! when an error is built, by scanning the raw request body along the current path
//! ([`locate`]); without a body, the value's compact JSON form stands in. Two locations exist,
//! as in Jackson: the token location (where the current token starts, used by Elasticsearch's
//! own exceptions) and the current location (just past it, used by Jackson's errors).
//!
//! [`XParser::object`] emulates `ObjectParser`: declared fields with their accepted token
//! types, unknown-field suggestions, deprecated names (warnings) and the
//! "failed to parse field" wrapping.

use std::cell::OnceCell;

use crate::error::{AggError, Result, suggest};
use crate::json::{Json, parse_java_double, parse_java_long};

/// One step of a path into a JSON document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// `XContentParser.Token`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    StartObject,
    EndObject,
    StartArray,
    EndArray,
    FieldName,
    String,
    Number,
    Boolean,
    Null,
}

impl Token {
    pub fn name(self) -> &'static str {
        match self {
            Token::StartObject => "START_OBJECT",
            Token::EndObject => "END_OBJECT",
            Token::StartArray => "START_ARRAY",
            Token::EndArray => "END_ARRAY",
            Token::FieldName => "FIELD_NAME",
            Token::String => "VALUE_STRING",
            Token::Number => "VALUE_NUMBER",
            Token::Boolean => "VALUE_BOOLEAN",
            Token::Null => "VALUE_NULL",
        }
    }

    /// `Token.isValue()`.
    pub fn is_value(self) -> bool {
        matches!(self, Token::String | Token::Number | Token::Boolean)
    }

    fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A set of tokens (`ObjectParser.ValueType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tokens(u16);

impl Tokens {
    pub const fn of(list: &[Token]) -> Tokens {
        let mut bits = 0u16;
        let mut i = 0;
        while i < list.len() {
            bits |= 1 << (list[i] as u16);
            i += 1;
        }
        Tokens(bits)
    }
    pub fn contains(self, t: Token) -> bool {
        self.0 & t.bit() != 0
    }

    pub const STRING: Tokens = Tokens::of(&[Token::String]);
    pub const STRING_OR_NULL: Tokens = Tokens::of(&[Token::String, Token::Null]);
    pub const FLOAT: Tokens = Tokens::of(&[Token::Number, Token::String]);
    pub const DOUBLE_OR_NULL: Tokens = Tokens::of(&[Token::Number, Token::String, Token::Null]);
    pub const INT: Tokens = Tokens::FLOAT;
    pub const LONG: Tokens = Tokens::FLOAT;
    pub const BOOLEAN: Tokens = Tokens::of(&[Token::Boolean, Token::String]);
    pub const OBJECT: Tokens = Tokens::of(&[Token::StartObject]);
    pub const OBJECT_OR_NULL: Tokens = Tokens::of(&[Token::StartObject, Token::Null]);
    pub const OBJECT_ARRAY: Tokens = Tokens::of(&[Token::StartObject, Token::StartArray]);
    pub const OBJECT_OR_STRING: Tokens = Tokens::of(&[Token::StartObject, Token::String]);
    pub const OBJECT_ARRAY_OR_STRING: Tokens = Tokens::of(&[Token::StartObject, Token::StartArray, Token::String]);
    pub const OBJECT_ARRAY_BOOLEAN_OR_STRING: Tokens =
        Tokens::of(&[Token::StartObject, Token::StartArray, Token::Boolean, Token::String]);
    pub const STRING_ARRAY: Tokens = Tokens::of(&[Token::StartArray, Token::String]);
    pub const FLOAT_ARRAY: Tokens = Tokens::of(&[Token::StartArray, Token::Number, Token::String]);
    pub const VALUE: Tokens = Tokens::of(&[Token::Boolean, Token::Null, Token::Number, Token::String]);
    pub const VALUE_ARRAY: Tokens = Tokens::of(&[Token::Boolean, Token::Null, Token::Number, Token::String, Token::StartArray]);
    pub const VALUE_OBJECT_ARRAY: Tokens =
        Tokens::of(&[Token::Boolean, Token::Null, Token::Number, Token::String, Token::StartObject, Token::StartArray]);
    pub const OBJECT_ARRAY_STRING_OR_NUMBER: Tokens =
        Tokens::of(&[Token::StartObject, Token::StartArray, Token::String, Token::Number]);
    pub const OBJECT_OR_BOOLEAN: Tokens = Tokens::of(&[Token::StartObject, Token::Boolean]);
    pub const ANY: Tokens = Tokens(u16::MAX);
}

/// One field an `ObjectParser` declares (`ParseField` plus its `ValueType`).
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub deprecated: &'static [&'static str],
    pub tokens: Tokens,
}

impl Field {
    pub const fn new(name: &'static str, tokens: Tokens) -> Field {
        Field { name, deprecated: &[], tokens }
    }
    pub const fn with_deprecated(name: &'static str, deprecated: &'static [&'static str], tokens: Tokens) -> Field {
        Field { name, deprecated, tokens }
    }
}

enum Frame<'a> {
    Obj { map: &'a [(String, Json)], container: &'a Json, next: usize, cur: Option<usize>, in_value: bool },
    Arr { arr: &'a [Json], container: &'a Json, next: usize, cur: Option<usize> },
}

/// A token stream over a JSON value with `XContentParser` semantics.
pub struct XParser<'a> {
    root: &'a Json,
    frames: Vec<Frame<'a>>,
    tok: Option<Token>,
    val: Option<&'a Json>,
    started: bool,
    finished: bool,
    src: Option<&'a [u8]>,
    owned_src: OnceCell<Vec<u8>>,
    base: Vec<Seg>,
    /// Deprecation warnings raised while parsing.
    pub warnings: Vec<String>,
}

impl<'a> XParser<'a> {
    /// A parser over `root`, which sits at `base` inside the raw body `src` (when known).
    pub fn new(root: &'a Json, src: Option<&'a [u8]>, base: Vec<Seg>) -> XParser<'a> {
        XParser {
            root,
            frames: Vec::new(),
            tok: None,
            val: None,
            started: false,
            finished: false,
            src,
            owned_src: OnceCell::new(),
            base,
            warnings: Vec::new(),
        }
    }

    fn token_of(v: &Json) -> Token {
        match v {
            Json::Obj(_) => Token::StartObject,
            Json::Arr(_) => Token::StartArray,
            Json::Str(_) => Token::String,
            Json::Int(_) | Json::UInt(_) | Json::Float(_) | Json::Float32(_) => Token::Number,
            Json::Bool(_) => Token::Boolean,
            Json::Null => Token::Null,
        }
    }

    /// `nextToken()`; `None` once the root value is consumed.
    pub fn next_token(&mut self) -> Option<Token> {
        if !self.started {
            self.started = true;
            self.tok = Some(Self::token_of(self.root));
            self.val = Some(self.root);
            return self.tok;
        }
        if self.finished {
            return None;
        }
        match (self.tok, self.val) {
            (Some(Token::StartObject), Some(v @ Json::Obj(map))) => {
                self.frames.push(Frame::Obj { map, container: v, next: 0, cur: None, in_value: false })
            }
            (Some(Token::StartArray), Some(v @ Json::Arr(arr))) => self.frames.push(Frame::Arr { arr, container: v, next: 0, cur: None }),
            _ => {}
        }
        let Some(top) = self.frames.last_mut() else {
            self.finished = true;
            self.tok = None;
            self.val = None;
            return None;
        };
        match top {
            Frame::Obj { map, container, next, cur, in_value } => {
                if *in_value {
                    *in_value = false;
                    let v = &map[cur.unwrap_or(0)].1;
                    self.tok = Some(Self::token_of(v));
                    self.val = Some(v);
                } else if *next < map.len() {
                    *cur = Some(*next);
                    *next += 1;
                    *in_value = true;
                    self.tok = Some(Token::FieldName);
                    self.val = None;
                } else {
                    let c = *container;
                    self.frames.pop();
                    self.tok = Some(Token::EndObject);
                    self.val = Some(c);
                }
            }
            Frame::Arr { arr, container, next, cur } => {
                if *next < arr.len() {
                    *cur = Some(*next);
                    *next += 1;
                    let v = &arr[*next - 1];
                    self.tok = Some(Self::token_of(v));
                    self.val = Some(v);
                } else {
                    let c = *container;
                    self.frames.pop();
                    self.tok = Some(Token::EndArray);
                    self.val = Some(c);
                }
            }
        }
        self.tok
    }

    pub fn current_token(&self) -> Option<Token> {
        self.tok
    }

    pub fn token(&self) -> Token {
        self.tok.unwrap_or(Token::EndObject)
    }

    /// The value of the current scalar token, or the container of a start/end token.
    pub fn value(&self) -> &'a Json {
        static NULL: Json = Json::Null;
        self.val.unwrap_or(&NULL)
    }

    /// `currentName()`.
    pub fn current_name(&self) -> Option<&'a str> {
        match self.frames.last()? {
            Frame::Obj { map, cur, .. } => cur.and_then(|i| map.get(i)).map(|(k, _)| k.as_str()),
            Frame::Arr { .. } => None,
        }
    }

    pub fn next_field_name(&mut self) -> Option<&'a str> {
        if self.next_token() == Some(Token::FieldName) { self.current_name() } else { None }
    }

    /// `skipChildren()`.
    pub fn skip_children(&mut self) {
        match self.tok {
            Some(Token::StartObject) => {
                self.tok = Some(Token::EndObject);
            }
            Some(Token::StartArray) => {
                self.tok = Some(Token::EndArray);
            }
            _ => {}
        }
    }

    // ---- locations ----

    fn source(&self) -> &[u8] {
        match self.src {
            Some(s) => s,
            None => self.owned_src.get_or_init(|| self.root.to_json_string().into_bytes()),
        }
    }

    /// The path of the current token from the body's root.
    pub fn path(&self) -> Vec<Seg> {
        let mut path = if self.src.is_some() { self.base.clone() } else { Vec::new() };
        for f in &self.frames {
            match f {
                Frame::Obj { map, cur: Some(i), .. } => {
                    if let Some((k, _)) = map.get(*i) {
                        path.push(Seg::Key(k.clone()));
                    }
                }
                Frame::Arr { cur: Some(i), .. } => path.push(Seg::Index(*i)),
                _ => {}
            }
        }
        path
    }

    /// Where the current token starts (`getTokenLocation()`).
    pub fn token_location(&self) -> (usize, usize) {
        let path = self.path();
        let src = self.source();
        let loc = match self.tok {
            Some(Token::FieldName) => locate(src, &path, Target::Name),
            Some(Token::EndObject | Token::EndArray) => value_end(src, &path),
            _ => locate(src, &path, Target::Value),
        };
        loc.unwrap_or((1, 1))
    }

    /// Just past the current token (Jackson's `getCurrentLocation()`).
    pub fn current_location(&self) -> (usize, usize) {
        let path = self.path();
        let src = self.source();
        let loc = match self.tok {
            Some(Token::FieldName) => locate(src, &path, Target::Name).map(|(l, c)| {
                let name = self.current_name().unwrap_or("");
                (l, c + name.len() + 2)
            }),
            Some(Token::StartObject | Token::StartArray) => locate(src, &path, Target::Value).map(|(l, c)| (l, c + 1)),
            _ => value_end(src, &path).map(|(l, c)| (l, c + 1)),
        };
        loc.unwrap_or((1, 1))
    }

    /// `ParsingException` at the current token.
    pub fn parsing_error(&self, msg: impl Into<String>) -> AggError {
        AggError::parsing(self.token_location(), msg)
    }

    /// `XContentParseException` at the current token.
    pub fn xcontent_error(&self, msg: impl AsRef<str>) -> AggError {
        AggError::x_content(self.token_location(), msg)
    }

    /// A Jackson exception at the current location, wrapped in `x_content_parse_exception`.
    pub fn jackson_error(&self, kind: &'static str, msg: &str) -> AggError {
        let (l, c) = self.current_location();
        let full = format!("{msg}\n at [Source: (byte[])\"\"; line: {l}, column: {c}]");
        AggError::x_content((l, c), &full).caused_by(AggError::new(400, kind, full))
    }

    fn jackson_token_name(&self) -> &'static str {
        match (self.tok, self.val) {
            (Some(Token::Boolean), Some(Json::Bool(true))) => "VALUE_TRUE",
            (Some(Token::Boolean), _) => "VALUE_FALSE",
            (Some(Token::Number), Some(Json::Float(_) | Json::Float32(_))) => "VALUE_NUMBER_FLOAT",
            (Some(Token::Number), _) => "VALUE_NUMBER_INT",
            (Some(t), _) => t.name(),
            (None, _) => "null",
        }
    }

    fn not_numeric(&self) -> AggError {
        self.jackson_error(
            "json_parse_exception",
            &format!("Current token ({}) not numeric, can not use numeric value accessors", self.jackson_token_name()),
        )
    }

    // ---- value accessors (AbstractXContentParser) ----

    /// `text()`.
    pub fn text(&self) -> Result<String> {
        match (self.tok, self.val) {
            (Some(Token::String), Some(Json::Str(s))) => Ok(s.clone()),
            (Some(Token::Number), Some(n)) => Ok(number_text(n)),
            (Some(Token::Boolean), Some(Json::Bool(b))) => Ok(b.to_string()),
            _ => {
                let (l, c) = self.token_location();
                Err(AggError::illegal_argument(format!(
                    "Expected text at {l}:{c} but found {}",
                    self.tok.map(Token::name).unwrap_or("null")
                )))
            }
        }
    }

    /// `textOrNull()`.
    pub fn text_or_null(&self) -> Result<Option<String>> {
        if self.tok == Some(Token::Null) { Ok(None) } else { self.text().map(Some) }
    }

    /// `objectText()`: a string, number or boolean as is; `None` for null.
    pub fn object_text(&self) -> Result<Option<Json>> {
        match self.tok {
            Some(Token::String | Token::Number | Token::Boolean) => Ok(self.val.cloned()),
            Some(Token::Null) => Ok(None),
            _ => self.text().map(|s| Some(Json::Str(s))),
        }
    }

    fn numeric_text(&self) -> Result<&'a str> {
        let Some(Json::Str(s)) = self.val else { return Ok("") };
        if s.len() > 1000 {
            return Err(AggError::illegal_argument(format!(
                "Numeric value length [{}] exceeds the maximum of [1000]",
                s.chars().count()
            )));
        }
        Ok(s.as_str())
    }

    /// `doubleValue()` with coercion from strings.
    pub fn double_value(&self) -> Result<f64> {
        match (self.tok, self.val) {
            (Some(Token::String), _) => {
                let s = self.numeric_text()?;
                java_parse_double(s)
            }
            (Some(Token::Number), Some(n)) => Ok(n.as_f64().unwrap_or(f64::NAN)),
            _ => Err(self.not_numeric()),
        }
    }

    /// `floatValue()`.
    pub fn float_value(&self) -> Result<f32> {
        self.double_value().map(|d| d as f32)
    }

    /// `intValue()` with coercion: strings through `Double.parseDouble`.
    pub fn int_value(&self) -> Result<i32> {
        match (self.tok, self.val) {
            (Some(Token::String), _) => {
                let s = self.numeric_text()?;
                if let Some(v) = parse_java_long(s)
                    && let Ok(i) = i32::try_from(v)
                {
                    return Ok(i);
                }
                let d = java_parse_double(s)?;
                if d < i32::MIN as f64 || d > i32::MAX as f64 {
                    return Err(AggError::illegal_argument(format!("Value [{s}] is out of range for an integer")));
                }
                Ok(d as i32)
            }
            (Some(Token::Number), Some(n)) => {
                let out_of_range = || {
                    self.jackson_error(
                        "input_coercion_exception",
                        &format!("Numeric value ({}) out of range of int (-2147483648 - 2147483647)", number_text(n)),
                    )
                };
                match n {
                    Json::Int(i) => i32::try_from(*i).map_err(|_| out_of_range()),
                    Json::Float(f) => {
                        if *f < i32::MIN as f64 || *f > i32::MAX as f64 || f.is_nan() {
                            Err(out_of_range())
                        } else {
                            Ok(*f as i32)
                        }
                    }
                    Json::Float32(f) => Ok(*f as i32),
                    _ => Err(out_of_range()),
                }
            }
            _ => Err(self.not_numeric()),
        }
    }

    /// `longValue()` with coercion.
    pub fn long_value(&self) -> Result<i64> {
        match (self.tok, self.val) {
            (Some(Token::String), _) => {
                let s = self.numeric_text()?;
                java_to_long(s)
            }
            (Some(Token::Number), Some(n)) => {
                let out_of_range = || {
                    self.jackson_error(
                        "input_coercion_exception",
                        &format!("Numeric value ({}) out of range of long (-9223372036854775808 - 9223372036854775807)", number_text(n)),
                    )
                };
                match n {
                    Json::Int(i) => Ok(*i),
                    Json::Float(f) => {
                        if *f < i64::MIN as f64 || *f > i64::MAX as f64 || f.is_nan() {
                            Err(out_of_range())
                        } else {
                            Ok(*f as i64)
                        }
                    }
                    Json::Float32(f) => Ok(*f as i64),
                    _ => Err(out_of_range()),
                }
            }
            _ => Err(self.not_numeric()),
        }
    }

    /// `booleanValue()`: booleans, or the strings `true`/`false`.
    pub fn boolean_value(&self) -> Result<bool> {
        match (self.tok, self.val) {
            (Some(Token::Boolean), Some(Json::Bool(b))) => Ok(*b),
            (Some(Token::String), Some(Json::Str(s))) => parse_boolean(s),
            _ => Err(self.jackson_error("json_parse_exception", &format!("Current token ({}) not of boolean type", self.jackson_token_name()))),
        }
    }

    /// `map()`: the current object (the parser moves to its end).
    pub fn map(&mut self) -> Result<Json> {
        match self.val {
            Some(m @ Json::Obj(_)) if self.tok == Some(Token::StartObject) => {
                let m = m.clone();
                self.skip_children();
                Ok(m)
            }
            _ => Err(self.jackson_error("json_parse_exception", &format!("Current token ({}) not START_OBJECT", self.jackson_token_name()))),
        }
    }

    /// The whole current value, leaving the parser on its end.
    pub fn take_value(&mut self) -> Json {
        let v = self.val.cloned().unwrap_or(Json::Null);
        self.skip_children();
        v
    }

    /// `list()`.
    pub fn list(&mut self) -> Result<Vec<Json>> {
        match self.val {
            Some(Json::Arr(a)) if self.tok == Some(Token::StartArray) => {
                let a = a.clone();
                self.skip_children();
                Ok(a)
            }
            _ => Err(self.jackson_error("json_parse_exception", &format!("Current token ({}) not START_ARRAY", self.jackson_token_name()))),
        }
    }

    /// `ObjectParser.parse`: walks the current (or next) object, checking each field against
    /// `fields` and calling `on(parser, field index)` positioned on its value. Unknown fields fail
    /// with suggestions; failures inside `on` are wrapped in "failed to parse field".
    pub fn object(&mut self, parser_name: &str, fields: &[Field], mut on: impl FnMut(&mut XParser<'a>, usize) -> Result<()>) -> Result<()> {
        if self.tok != Some(Token::StartObject) {
            let t = self.next_token();
            if t != Some(Token::StartObject) {
                return Err(self.xcontent_error(format!("[{parser_name}] Expected START_OBJECT but was: {}", t.map(Token::name).unwrap_or("null"))));
            }
        }
        while let Some(name) = self.next_field_name() {
            let name_loc = self.token_location();
            let idx = fields.iter().position(|f| f.name == name || f.deprecated.contains(&name));
            let token = self.next_token().unwrap_or(Token::EndObject);
            let Some(idx) = idx else {
                let mut candidates: Vec<&str> = Vec::new();
                for f in fields {
                    candidates.push(f.name);
                    candidates.extend(f.deprecated.iter().copied());
                }
                return Err(AggError::x_content(name_loc, format!("[{parser_name}] unknown field [{name}]{}", suggest(name, &candidates))));
            };
            let field = fields[idx];
            if field.deprecated.contains(&name) {
                let (l, c) = self.token_location();
                self.warnings.push(format!("[{parser_name}][{l}:{c}] Deprecated field [{name}] used, expected [{}] instead", field.name));
            }
            if !field.tokens.contains(token) {
                return Err(self.xcontent_error(format!("[{parser_name}] {name} doesn't support values of type: {token}")));
            }
            if let Err(e) = on(self, idx) {
                return Err(self.xcontent_error(format!("[{parser_name}] failed to parse field [{name}]")).caused_by(e));
            }
            match token {
                Token::StartObject if self.tok != Some(Token::EndObject) => {
                    return Err(self.xcontent_error(format!("parser for [{name}] did not end on END_OBJECT")));
                }
                Token::StartArray if self.tok != Some(Token::EndArray) => {
                    return Err(self.xcontent_error(format!("parser for [{name}] did not end on END_ARRAY")));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `AbstractObjectParser.parseArray`: one value, or each item of an array.
    pub fn array_items<T>(&mut self, mut item: impl FnMut(&mut XParser<'a>) -> Result<T>) -> Result<Vec<T>> {
        let t = self.token();
        if t.is_value() || t == Token::Null || t == Token::StartObject {
            return Ok(vec![item(self)?]);
        }
        let mut out = Vec::new();
        while let Some(t) = self.next_token() {
            if t == Token::EndArray {
                break;
            }
            if t.is_value() || t == Token::Null || t == Token::StartObject {
                out.push(item(self)?);
            } else {
                return Err(AggError::illegal_state(format!("expected value but got [{t}]")));
            }
        }
        Ok(out)
    }
}

/// A number's text as Jackson gives it for `text()`.
fn number_text(n: &Json) -> String {
    match n {
        Json::Int(i) => i.to_string(),
        Json::UInt(u) => u.to_string(),
        Json::Float(f) => crate::json::java_double(*f),
        Json::Float32(f) => crate::json::java_float(*f),
        other => other.to_java_string(),
    }
}

/// `Booleans.parseBoolean(String)`.
pub fn parse_boolean(s: &str) -> Result<bool> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AggError::illegal_argument(format!("Failed to parse value [{s}] as only [true] or [false] are allowed."))),
    }
}

/// `Double.parseDouble` with Java's `NumberFormatException` texts.
pub fn java_parse_double(s: &str) -> Result<f64> {
    if s.trim().is_empty() {
        return Err(AggError::new(400, "number_format_exception", "empty String"));
    }
    parse_java_double(s).ok_or_else(|| AggError::number_format(s))
}

/// `AbstractXContentParser.toLong` with coercion: integers, or decimals truncated.
pub fn java_to_long(s: &str) -> Result<i64> {
    if let Some(v) = parse_java_long(s) {
        return Ok(v);
    }
    let out_of_range = || AggError::illegal_argument(format!("Value [{s}] is out of range for a long"));
    let Some(d) = parse_java_double(s).filter(|_| !s.trim().is_empty() && s.trim() == s) else {
        return Err(AggError::number_format(s));
    };
    if !d.is_finite() {
        return Err(AggError::number_format(s));
    }
    let t = d.trunc();
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&t) {
        return Err(out_of_range());
    }
    Ok(t as i64)
}

/// What to locate at the end of a path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    Name,
    Value,
}

struct Scanner<'a> {
    s: &'a [u8],
    pos: usize,
    line: usize,
    row_start: usize,
}

impl Scanner<'_> {
    fn loc(&self, at: usize) -> (usize, usize) {
        (self.line, at - self.row_start + 1)
    }

    fn ws(&mut self) {
        while self.pos < self.s.len() {
            match self.s[self.pos] {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => {
                    self.pos += 1;
                    self.line += 1;
                    self.row_start = self.pos;
                }
                b'/' if self.s.get(self.pos + 1) == Some(&b'*') => {
                    self.pos += 2;
                    while self.pos + 1 < self.s.len() && !(self.s[self.pos] == b'*' && self.s[self.pos + 1] == b'/') {
                        if self.s[self.pos] == b'\n' {
                            self.line += 1;
                            self.row_start = self.pos + 1;
                        }
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.s.len());
                }
                b'/' if self.s.get(self.pos + 1) == Some(&b'/') => {
                    while self.pos < self.s.len() && self.s[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.s.get(self.pos) != Some(&b'"') {
            return None;
        }
        self.pos += 1;
        let mut out = Vec::new();
        while self.pos < self.s.len() {
            let c = self.s[self.pos];
            match c {
                b'"' => {
                    self.pos += 1;
                    return String::from_utf8(out).ok();
                }
                b'\\' => {
                    let e = *self.s.get(self.pos + 1)?;
                    self.pos += 2;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex = std::str::from_utf8(self.s.get(self.pos..self.pos + 4)?).ok()?;
                            let cp = u32::from_str_radix(hex, 16).ok()?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(char::from_u32(cp).unwrap_or('\u{FFFD}').encode_utf8(&mut buf).as_bytes());
                            self.pos += 4;
                        }
                        other => out.push(other),
                    }
                }
                _ => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
        None
    }

    /// Skips one value (iteratively: bodies may nest deeply).
    fn skip(&mut self) -> Option<()> {
        let mut depth = 0usize;
        loop {
            self.ws();
            match *self.s.get(self.pos)? {
                b'"' => {
                    self.string()?;
                }
                b'{' | b'[' => {
                    self.pos += 1;
                    depth += 1;
                    continue;
                }
                b'}' | b']' => {
                    self.pos += 1;
                    depth = depth.checked_sub(1)?;
                }
                b',' | b':' => {
                    self.pos += 1;
                    continue;
                }
                _ => {
                    while self.pos < self.s.len() && !matches!(self.s[self.pos], b',' | b'}' | b']' | b' ' | b'\n' | b'\t' | b'\r' | b':') {
                        self.pos += 1;
                    }
                }
            }
            if depth == 0 {
                return Some(());
            }
        }
    }
}

/// Finds the location of a field name or value along `path`.
pub fn locate(src: &[u8], path: &[Seg], target: Target) -> Option<(usize, usize)> {
    let mut sc = Scanner { s: src, pos: 0, line: 1, row_start: 0 };
    sc.ws();
    for (depth, seg) in path.iter().enumerate() {
        let last = depth + 1 == path.len();
        match seg {
            Seg::Key(k) => {
                if sc.s.get(sc.pos) != Some(&b'{') {
                    return None;
                }
                sc.pos += 1;
                loop {
                    sc.ws();
                    if sc.s.get(sc.pos) == Some(&b'}') {
                        return None;
                    }
                    let name_at = sc.pos;
                    let name_loc = sc.loc(name_at);
                    let name = sc.string()?;
                    sc.ws();
                    if sc.s.get(sc.pos) != Some(&b':') {
                        return None;
                    }
                    sc.pos += 1;
                    sc.ws();
                    if name == *k {
                        if last && target == Target::Name {
                            return Some(name_loc);
                        }
                        break;
                    }
                    sc.skip()?;
                    sc.ws();
                    match sc.s.get(sc.pos)? {
                        b',' => sc.pos += 1,
                        _ => return None,
                    }
                }
            }
            Seg::Index(n) => {
                if sc.s.get(sc.pos) != Some(&b'[') {
                    return None;
                }
                sc.pos += 1;
                for _ in 0..*n {
                    sc.skip()?;
                    sc.ws();
                    if sc.s.get(sc.pos) != Some(&b',') {
                        return None;
                    }
                    sc.pos += 1;
                }
                sc.ws();
            }
        }
    }
    sc.ws();
    Some(sc.loc(sc.pos))
}

/// Location of the last byte of the value at `path`.
pub fn value_end(src: &[u8], path: &[Seg]) -> Option<(usize, usize)> {
    let (line, col) = locate(src, path, Target::Value)?;
    let mut sc = Scanner { s: src, pos: 0, line: 1, row_start: 0 };
    let mut l = 1;
    while l < line {
        if sc.s.get(sc.pos)? == &b'\n' {
            l += 1;
            sc.row_start = sc.pos + 1;
        }
        sc.pos += 1;
    }
    sc.line = line;
    sc.pos = sc.row_start + col - 1;
    sc.skip()?;
    let end = sc.pos.checked_sub(1)?;
    let mut end_line = line;
    let mut end_row = sc.row_start;
    for (i, b) in src[sc.row_start..=end].iter().enumerate() {
        if *b == b'\n' {
            end_line += 1;
            end_row = sc.row_start + i + 1;
        }
    }
    Some((end_line, end - end_row + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_and_unknown_fields() {
        let src = r#"{"aggs":{"a":{"terms":{"fieldx":"x"}}}}"#;
        let body = Json::parse(src).unwrap();
        let terms = body.get("aggs").unwrap().get("a").unwrap().get("terms").unwrap();
        let base = vec![Seg::Key("aggs".into()), Seg::Key("a".into()), Seg::Key("terms".into())];
        let mut p = XParser::new(terms, Some(src.as_bytes()), base);
        let e = p.object("terms", &[Field::new("field", Tokens::STRING)], |_, _| Ok(())).unwrap_err();
        assert_eq!(e.reason, "[1:24] [terms] unknown field [fieldx] did you mean [field]?");
    }

    #[test]
    fn number_coercion() {
        assert_eq!(java_to_long("1.9").unwrap(), 1);
        assert_eq!(java_parse_double("abc").unwrap_err().reason, "For input string: \"abc\"");
    }
}
