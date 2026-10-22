//! A pull parser over a parsed JSON value with the semantics of Elasticsearch's
//! `XContentParser`, so the Query DSL parsers can follow Elasticsearch's token-by-token logic and
//! fail with the same exceptions, texts and `[line:col]` locations.
//!
//! Request bodies are parsed once into [`Value`]s. Query parsing then walks that tree as a token
//! stream (`START_OBJECT`, `FIELD_NAME`, `VALUE_STRING`, …) instead of re-tokenizing bytes, which
//! keeps the walk allocation-free. Locations are only computed when an error is built: the path of
//! the current token is located in the raw request body ([`crate::json::locate`]), or in the
//! value's compact JSON form when there is no body (stored alias filters). Two kinds of location
//! exist, as in Jackson: the *token* location (where the current token starts, used by
//! Elasticsearch's own exceptions) and the *current* location (just past the current token, used
//! by Jackson's own errors such as "not numeric").
//!
//! The value accessors reproduce `AbstractXContentParser`: numbers coerce from strings with Java's
//! parsing rules, booleans accept `"true"`/`"false"` strings, `text()` refuses non-values, and
//! wrong token types fail with Jackson's messages wrapped in `x_content_parse_exception`.
//! [`XParser::object`] emulates `ObjectParser`: declared fields with their accepted token types,
//! unknown-field suggestions, deprecated names and the "failed to parse field" wrapping.

use crate::error::{EsError, EsResult};
use crate::json::locate::{Seg, Target, locate, value_end};
use crate::json::{Map, Number, Value};
use std::cell::OnceCell;

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
    /// The XContent token name.
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

    /// `Token.isValue()`: strings, numbers and booleans (not `null`).
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
    pub const INT: Tokens = Tokens::FLOAT;
    pub const BOOLEAN: Tokens = Tokens::of(&[Token::Boolean, Token::String]);
    pub const OBJECT: Tokens = Tokens::of(&[Token::StartObject]);
    pub const OBJECT_OR_NULL: Tokens = Tokens::of(&[Token::StartObject, Token::Null]);
    pub const OBJECT_ARRAY: Tokens = Tokens::of(&[Token::StartObject, Token::StartArray]);
    pub const OBJECT_OR_STRING: Tokens = Tokens::of(&[Token::StartObject, Token::String]);
    pub const OBJECT_ARRAY_OR_STRING: Tokens = Tokens::of(&[Token::StartObject, Token::StartArray, Token::String]);
    pub const STRING_ARRAY: Tokens = Tokens::of(&[Token::StartArray, Token::String]);
    pub const FLOAT_ARRAY: Tokens = Tokens::of(&[Token::StartArray, Token::Number, Token::String]);
    pub const VALUE: Tokens = Tokens::of(&[Token::Boolean, Token::Null, Token::Number, Token::String]);
    pub const VALUE_ARRAY: Tokens = Tokens::of(&[Token::Boolean, Token::Null, Token::Number, Token::String, Token::StartArray]);
    pub const VALUE_OBJECT_ARRAY: Tokens =
        Tokens::of(&[Token::Boolean, Token::Null, Token::Number, Token::String, Token::StartObject, Token::StartArray]);
    pub const OBJECT_ARRAY_STRING_OR_NUMBER: Tokens = Tokens::of(&[Token::StartObject, Token::StartArray, Token::String, Token::Number]);
    pub const OBJECT_OR_BOOLEAN: Tokens = Tokens::of(&[Token::StartObject, Token::Boolean]);
    pub const ANY: Tokens = Tokens(u16::MAX);
}

/// One field an [`XParser::object`] parser declares (`ParseField` plus its `ValueType`).
#[derive(Clone, Copy, Debug)]
pub struct Field {
    /// The preferred name.
    pub name: &'static str,
    /// Deprecated alternative names (`ParseField.withDeprecation`).
    pub deprecated: &'static [&'static str],
    pub tokens: Tokens,
    /// `ParseField.withAllDeprecated`: every name is deprecated, with this replacement (or none).
    pub all_deprecated: Option<&'static str>,
}

impl Field {
    pub const fn new(name: &'static str, tokens: Tokens) -> Field {
        Field { name, deprecated: &[], tokens, all_deprecated: None }
    }
    pub const fn with_deprecated(name: &'static str, deprecated: &'static [&'static str], tokens: Tokens) -> Field {
        Field { name, deprecated, tokens, all_deprecated: None }
    }
}

enum Frame<'a> {
    Obj { map: &'a Map, container: &'a Value, next: usize, cur: Option<usize>, in_value: bool },
    Arr { arr: &'a [Value], container: &'a Value, next: usize, cur: Option<usize> },
}

/// A token stream over a JSON value with `XContentParser` semantics.
pub struct XParser<'a> {
    root: &'a Value,
    frames: Vec<Frame<'a>>,
    tok: Option<Token>,
    /// The value of the current token: the scalar, or the container of a start/end token.
    val: Option<&'a Value>,
    started: bool,
    finished: bool,
    src: Option<&'a [u8]>,
    owned_src: OnceCell<Vec<u8>>,
    base: Vec<Seg<'a>>,
    /// Deprecation warnings raised while parsing (Elasticsearch's `Warning` header texts).
    pub warnings: Vec<String>,
    /// Query nesting depth (`indices.query.bool.max_nested_depth`).
    pub depth: usize,
    pub max_depth: usize,
    /// Jackson describes the input as `(String)` rather than `(byte[])` (stored alias filters
    /// are re-parsed from their string form).
    pub string_source: bool,
}

impl<'a> XParser<'a> {
    /// A parser over `root`, which sits at `base` inside the raw body `src` (when known).
    pub fn new(root: &'a Value, src: Option<&'a [u8]>, base: Vec<Seg<'a>>) -> XParser<'a> {
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
            depth: 0,
            max_depth: 30,
            string_source: false,
        }
    }

    /// A parser over a value without a raw body: locations refer to its compact JSON form.
    pub fn from_value(root: &'a Value) -> XParser<'a> {
        XParser::new(root, None, Vec::new())
    }

    /// A parser positioned on `root`'s first token, as Elasticsearch hands a parser to a query
    /// parser after reading the value's start.
    pub fn started(root: &'a Value, src: Option<&'a [u8]>, base: Vec<Seg<'a>>) -> XParser<'a> {
        let mut p = XParser::new(root, src, base);
        p.next_token();
        p
    }

    fn token_of(v: &Value) -> Token {
        match v {
            Value::Obj(_) => Token::StartObject,
            Value::Arr(_) => Token::StartArray,
            Value::Str(_) => Token::String,
            Value::Num(_) => Token::Number,
            Value::Bool(_) => Token::Boolean,
            Value::Null => Token::Null,
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
        // Entering the container the current start token opened.
        match (self.tok, self.val) {
            (Some(Token::StartObject), Some(v @ Value::Obj(map))) => {
                self.frames.push(Frame::Obj { map, container: v, next: 0, cur: None, in_value: false })
            }
            (Some(Token::StartArray), Some(v @ Value::Arr(arr))) => {
                self.frames.push(Frame::Arr { arr, container: v, next: 0, cur: None })
            }
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
                    let v = &map[cur.unwrap_or(0)];
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
        if self.frames.is_empty() && matches!(self.tok, Some(Token::EndObject | Token::EndArray)) {
            // The root container closed; the next call reports the end of input.
        }
        self.tok
    }

    /// `currentToken()`.
    pub fn current_token(&self) -> Option<Token> {
        self.tok
    }

    /// The current token, which must exist (parsers only ask while inside a value).
    pub fn token(&self) -> Token {
        self.tok.unwrap_or(Token::EndObject)
    }

    /// The value of the current scalar token, or the container of a start/end token.
    pub fn value(&self) -> &'a Value {
        static NULL: Value = Value::Null;
        self.val.unwrap_or(&NULL)
    }

    /// `currentName()`: the field name for a field name or the value after it, the closed
    /// container's field name for an end token; `None` inside arrays.
    pub fn current_name(&self) -> Option<&'a str> {
        match self.frames.last()? {
            Frame::Obj { map, cur, .. } => cur.and_then(|i| map.get_index(i)).map(|(k, _)| k.as_str()),
            Frame::Arr { .. } => None,
        }
    }

    /// `nextFieldName()`: advances and returns the field name, or `None` (on any other token).
    pub fn next_field_name(&mut self) -> Option<&'a str> {
        if self.next_token() == Some(Token::FieldName) { self.current_name() } else { None }
    }

    /// `skipChildren()`: from a start token to its matching end token.
    pub fn skip_children(&mut self) {
        match self.tok {
            Some(Token::StartObject) => self.tok = Some(Token::EndObject),
            Some(Token::StartArray) => self.tok = Some(Token::EndArray),
            _ => {}
        }
    }

    // ---- locations ----

    fn source(&self) -> &[u8] {
        match self.src {
            Some(s) => s,
            None => self.owned_src.get_or_init(|| {
                let mut out = Vec::new();
                crate::json::write::write_json(&mut out, self.root, false);
                out
            }),
        }
    }

    fn path(&self) -> Vec<Seg<'a>> {
        let mut path = if self.src.is_some() { self.base.clone() } else { Vec::new() };
        for f in &self.frames {
            match f {
                Frame::Obj { map, cur: Some(i), .. } => {
                    if let Some((k, _)) = map.get_index(*i) {
                        path.push(Seg::Key(k.as_str()));
                    }
                }
                Frame::Arr { cur: Some(i), .. } => path.push(Seg::Index(*i)),
                _ => {}
            }
        }
        path
    }

    /// The location of a value below the current token (`rel` is relative to it).
    pub fn location_of(&self, rel: &[Seg<'_>]) -> (usize, usize) {
        let mut path: Vec<Seg<'_>> = self.path();
        path.extend_from_slice(rel);
        locate(self.source(), &path, Target::Value).unwrap_or_else(|| self.token_location())
    }

    /// The current scalar's text as it appears in the body (Jackson keeps a number's original
    /// digits: `1e19`, not `1.0E19`).
    pub fn raw_text(&self) -> String {
        if let (Some(src), Some(Token::Number)) = (self.src, self.tok)
            && let Some((a, b)) = crate::json::locate::value_span(src, &self.path())
            && let Ok(t) = std::str::from_utf8(&src[a..b])
        {
            return t.to_string();
        }
        self.value().to_es_string()
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
    pub fn parsing_error(&self, msg: impl Into<String>) -> EsError {
        let (l, c) = self.token_location();
        EsError::parsing(l, c, msg)
    }

    /// `XContentParseException` at the current token.
    pub fn xcontent_error(&self, msg: impl Into<String>) -> EsError {
        let (l, c) = self.token_location();
        EsError::x_content_parse(l, c, msg)
    }

    /// A Jackson exception (`kind`, e.g. `json_parse_exception`) at the current location, wrapped
    /// in `x_content_parse_exception` as Elasticsearch's JSON parser wraps it.
    pub fn jackson_error(&self, kind: &'static str, msg: &str) -> EsError {
        self.jackson_error_at(self.current_location(), kind, msg)
    }

    /// [`Self::jackson_error`] at a given location.
    pub fn jackson_error_at(&self, (l, c): (usize, usize), kind: &'static str, msg: &str) -> EsError {
        let mut loc = crate::json::parse::source_location(self.source(), l, c);
        if self.string_source {
            loc = loc.replacen("(byte[])", "(String)", 1);
        }
        let full = format!("{msg}\n at {loc}");
        EsError::x_content_parse(l, c, full.clone()).caused_by(EsError::new(400, kind, full))
    }

    /// Jackson's name for the current token in its own messages.
    fn jackson_token_name(&self) -> &'static str {
        match (self.tok, self.val) {
            (Some(Token::Boolean), Some(Value::Bool(true))) => "VALUE_TRUE",
            (Some(Token::Boolean), _) => "VALUE_FALSE",
            (Some(Token::Number), Some(Value::Num(Number::Float(_)))) => "VALUE_NUMBER_FLOAT",
            (Some(Token::Number), _) => "VALUE_NUMBER_INT",
            (Some(t), _) => t.name(),
            (None, _) => "null",
        }
    }

    fn not_numeric(&self) -> EsError {
        self.jackson_error(
            "json_parse_exception",
            &format!("Current token ({}) not numeric, can not use numeric value accessors", self.jackson_token_name()),
        )
    }

    // ---- value accessors (AbstractXContentParser) ----

    /// `text()`: the textual form of a string, number or boolean.
    pub fn text(&self) -> EsResult<String> {
        match self.val {
            Some(Value::Str(s)) if self.tok == Some(Token::String) => Ok(s.clone()),
            Some(Value::Num(n)) if self.tok == Some(Token::Number) => Ok(n.to_string()),
            Some(Value::Bool(b)) if self.tok == Some(Token::Boolean) => Ok(b.to_string()),
            _ => {
                let (l, c) = self.token_location();
                Err(EsError::illegal_argument(format!(
                    "Expected text at {l}:{c} but found {}",
                    self.tok.map(Token::name).unwrap_or("null")
                )))
            }
        }
    }

    /// `textOrNull()`.
    pub fn text_or_null(&self) -> EsResult<Option<String>> {
        if self.tok == Some(Token::Null) { Ok(None) } else { self.text().map(Some) }
    }

    /// `objectText()`: a string, number or boolean as is; `None` for null.
    pub fn object_text(&self) -> EsResult<Option<Value>> {
        match self.tok {
            Some(Token::String | Token::Number | Token::Boolean) => Ok(self.val.cloned()),
            Some(Token::Null) => Ok(None),
            _ => self.text().map(|s| Some(Value::Str(s))),
        }
    }

    /// `objectBytes()`: like [`Self::object_text`], but a container reads as Jackson's text for
    /// its start token and stays unconsumed.
    pub fn object_bytes(&self) -> Option<Value> {
        match self.tok {
            Some(Token::String | Token::Number | Token::Boolean) => self.val.cloned(),
            Some(Token::Null) => None,
            Some(Token::StartObject) => Some(Value::Str("{".into())),
            Some(Token::StartArray) => Some(Value::Str("[".into())),
            Some(Token::EndObject) => Some(Value::Str("}".into())),
            Some(Token::EndArray) => Some(Value::Str("]".into())),
            Some(Token::FieldName) => self.current_name().map(|n| Value::Str(n.to_string())),
            None => None,
        }
    }

    fn numeric_text(&self) -> EsResult<&'a str> {
        let Some(Value::Str(s)) = self.val else { return Ok("") };
        if s.len() > 1000 {
            return Err(EsError::illegal_argument(format!(
                "Numeric value length [{}] exceeds the maximum of [1000]",
                s.chars().count()
            )));
        }
        Ok(s.as_str())
    }

    /// `doubleValue()` with coercion from strings.
    pub fn double_value(&self) -> EsResult<f64> {
        match (self.tok, self.val) {
            (Some(Token::String), _) => {
                let s = self.numeric_text()?;
                java_parse_double(s)
            }
            (Some(Token::Number), Some(Value::Num(n))) => Ok(n.as_f64()),
            _ => Err(self.not_numeric()),
        }
    }

    /// `floatValue()` with coercion from strings.
    pub fn float_value(&self) -> EsResult<f32> {
        match (self.tok, self.val) {
            (Some(Token::String), _) => {
                let s = self.numeric_text()?;
                java_parse_double(s).map(|d| d as f32)
            }
            (Some(Token::Number), Some(Value::Num(n))) => Ok(n.as_f64() as f32),
            _ => Err(self.not_numeric()),
        }
    }

    /// `intValue()` with coercion: strings through `Double.parseDouble`, out-of-range numbers fail
    /// as Jackson's coercion does.
    pub fn int_value(&self) -> EsResult<i32> {
        match (self.tok, self.val) {
            (Some(Token::String), _) => {
                let s = self.numeric_text()?;
                let d = java_parse_double(s)?;
                if d < i32::MIN as f64 || d > i32::MAX as f64 {
                    return Err(EsError::illegal_argument(format!("Value [{s}] is out of range for an integer")));
                }
                Ok(d as i32)
            }
            (Some(Token::Number), Some(Value::Num(n))) => {
                let out_of_range = || {
                    self.jackson_error(
                        "input_coercion_exception",
                        &format!("Numeric value ({}) out of range of int (-2147483648 - 2147483647)", self.raw_text()),
                    )
                };
                match n {
                    Number::Int(i) => i32::try_from(*i).map_err(|_| out_of_range()),
                    Number::Float(f) => {
                        if *f < i32::MIN as f64 || *f > i32::MAX as f64 || f.is_nan() {
                            Err(out_of_range())
                        } else {
                            Ok(*f as i32)
                        }
                    }
                    _ => Err(out_of_range()),
                }
            }
            _ => Err(self.not_numeric()),
        }
    }

    /// `longValue()` with coercion.
    pub fn long_value(&self) -> EsResult<i64> {
        match (self.tok, self.val) {
            (Some(Token::String), _) => {
                let s = self.numeric_text()?;
                java_to_long(s)
            }
            (Some(Token::Number), Some(Value::Num(n))) => {
                let out_of_range = || {
                    self.jackson_error(
                        "input_coercion_exception",
                        &format!("Numeric value ({}) out of range of long (-9223372036854775808 - 9223372036854775807)", self.raw_text()),
                    )
                };
                match n {
                    Number::Int(i) => Ok(*i),
                    Number::Float(f) => {
                        if *f < i64::MIN as f64 || *f > i64::MAX as f64 || f.is_nan() {
                            Err(out_of_range())
                        } else {
                            Ok(*f as i64)
                        }
                    }
                    _ => Err(out_of_range()),
                }
            }
            _ => Err(self.not_numeric()),
        }
    }

    /// `booleanValue()`: booleans, or the strings `true`/`false`.
    pub fn boolean_value(&self) -> EsResult<bool> {
        match (self.tok, self.val) {
            (Some(Token::Boolean), Some(Value::Bool(b))) => Ok(*b),
            (Some(Token::String), Some(Value::Str(s))) => parse_boolean(s),
            _ => Err(self.jackson_error(
                "json_parse_exception",
                &format!("Current token ({}) not of boolean type", self.jackson_token_name()),
            )),
        }
    }

    /// `isBooleanValue()`.
    pub fn is_boolean_value(&self) -> bool {
        match (self.tok, self.val) {
            (Some(Token::Boolean), _) => true,
            (Some(Token::String), Some(Value::Str(s))) => s == "true" || s == "false",
            _ => false,
        }
    }

    /// `map()`/`mapOrdered()`: the current object as a map (the parser moves to its end).
    pub fn map(&mut self) -> EsResult<Map> {
        match self.val {
            Some(Value::Obj(m)) if self.tok == Some(Token::StartObject) => {
                let m = m.clone();
                self.skip_children();
                Ok(m)
            }
            _ => Err(self.jackson_error(
                "json_parse_exception",
                &format!("Current token ({}) not START_OBJECT", self.jackson_token_name()),
            )),
        }
    }

    /// The whole current value (an object, array or scalar), leaving the parser on its end.
    pub fn take_value(&mut self) -> Value {
        let v = self.val.cloned().unwrap_or(Value::Null);
        self.skip_children();
        v
    }

    /// `list()`/`listOrderedMap()`: the current array's values.
    pub fn list(&mut self) -> EsResult<Vec<Value>> {
        match self.val {
            Some(Value::Arr(a)) if self.tok == Some(Token::StartArray) => {
                let a = a.clone();
                self.skip_children();
                Ok(a)
            }
            _ => Err(self.jackson_error(
                "json_parse_exception",
                &format!("Current token ({}) not START_ARRAY", self.jackson_token_name()),
            )),
        }
    }

    /// Records the deprecation warning `ParseField.match` logs for a deprecated name. Matches
    /// made by an `ObjectParser` name the parser and the field's location first.
    pub fn deprecated_name(&mut self, used: &str, preferred: &str, ctx: Option<(&str, (usize, usize))>) {
        let prefix = deprecation_prefix(ctx);
        self.warnings.push(format!("{prefix}Deprecated field [{used}] used, expected [{preferred}] instead"));
    }

    /// `ParseField.match`: whether `name` is `field`'s preferred or a deprecated name (warning
    /// for the latter).
    pub fn matches(&mut self, name: &str, field: &Field) -> bool {
        self.matches_in(name, field, None)
    }

    fn matches_in(&mut self, name: &str, field: &Field, ctx: Option<(&str, (usize, usize))>) -> bool {
        if name == field.name {
            if let Some(repl) = field.all_deprecated {
                self.all_deprecated(name, repl, ctx);
            }
            return true;
        }
        if field.deprecated.contains(&name) {
            match field.all_deprecated {
                Some(repl) => self.all_deprecated(name, repl, ctx),
                None => self.deprecated_name(name, field.name, ctx),
            }
            return true;
        }
        false
    }

    fn all_deprecated(&mut self, name: &str, replacement: &str, ctx: Option<(&str, (usize, usize))>) {
        let prefix = deprecation_prefix(ctx);
        if replacement.is_empty() {
            self.warnings.push(format!("{prefix}Deprecated field [{name}] used, this field is unused and will be removed entirely"));
        } else {
            self.warnings.push(format!("{prefix}Deprecated field [{name}] used, replaced by [{replacement}]"));
        }
    }

    /// `ObjectParser.parse`: walks the current (or next) object, checking each field against
    /// `fields` and calling `on(parser, field index)` positioned on its value. Unknown fields fail
    /// with suggestions; failures inside `on` are wrapped in "failed to parse field".
    pub fn object(
        &mut self,
        parser_name: &str,
        fields: &[Field],
        mut on: impl FnMut(&mut XParser<'a>, usize) -> EsResult<()>,
    ) -> EsResult<()> {
        if self.tok != Some(Token::StartObject) {
            let t = self.next_token();
            if t != Some(Token::StartObject) {
                return Err(self.xcontent_error(format!(
                    "[{parser_name}] Expected START_OBJECT but was: {}",
                    t.map(Token::name).unwrap_or("null")
                )));
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
                return Err(EsError::x_content_parse(
                    name_loc.0,
                    name_loc.1,
                    format!("[{parser_name}] unknown field [{name}]{}", crate::rest::params::suggest(name, &candidates)),
                ));
            };
            let field = fields[idx];
            // `ParseField.match` gets the location supplier after the value token was read.
            let value_loc = self.token_location();
            self.matches_in(name, &field, Some((parser_name, value_loc)));
            if !field.tokens.contains(token) {
                return Err(self.xcontent_error(format!("[{parser_name}] {name} doesn't support values of type: {token}")));
            }
            match token {
                Token::EndObject | Token::EndArray | Token::FieldName => {
                    return Err(self.xcontent_error(format!("[{parser_name}]{token} is unexpected")));
                }
                _ => {}
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

    /// `AbstractObjectParser.parseArray`: one item, or each item of an array, through `item`.
    pub fn array_items<T>(&mut self, mut item: impl FnMut(&mut XParser<'a>) -> EsResult<T>) -> EsResult<Vec<T>> {
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
                return Err(EsError::illegal_state(format!("expected value but got [{t}]")));
            }
        }
        Ok(out)
    }
}

fn deprecation_prefix(ctx: Option<(&str, (usize, usize))>) -> String {
    match ctx {
        Some((parser, (l, c))) => format!("[{parser}][{l}:{c}] "),
        None => String::new(),
    }
}

/// `Booleans.parseBoolean(String)`: only `true` and `false`.
pub fn parse_boolean(s: &str) -> EsResult<bool> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(EsError::illegal_argument(format!("Failed to parse value [{s}] as only [true] or [false] are allowed."))),
    }
}

/// `Double.parseDouble` with Java's `NumberFormatException` texts.
pub fn java_parse_double(s: &str) -> EsResult<f64> {
    if s.trim().is_empty() {
        return Err(EsError::java(400, "number_format_exception", "empty String"));
    }
    crate::units::parse_java_double(s).ok_or_else(|| EsError::number_format(s))
}

/// `Float.parseFloat`.
pub fn java_parse_float(s: &str) -> EsResult<f32> {
    java_parse_double(s).map(|d| d as f32)
}

/// `AbstractXContentParser.toLong` with coercion: integers, or decimals truncated.
fn java_to_long(s: &str) -> EsResult<i64> {
    if let Some(v) = crate::units::parse_java_long(s) {
        return Ok(v);
    }
    let out_of_range = || EsError::illegal_argument(format!("Value [{s}] is out of range for a long"));
    let Some(d) = crate::units::parse_java_double(s).filter(|_| !s.trim().is_empty() && s.trim() == s) else {
        return Err(EsError::illegal_argument(format!("For input string: \"{s}\"")));
    };
    if !d.is_finite() {
        return Err(EsError::illegal_argument(format!("For input string: \"{s}\"")));
    }
    let t = d.trunc();
    // 2^63 is exact as a double: anything at or above it (or below -2^63) doesn't fit.
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&t) {
        return Err(out_of_range());
    }
    Ok(t as i64)
}

/// A Java `float` as Elasticsearch prints it (`Float.toString`).
pub fn float_value(f: f32) -> Value {
    let text = crate::json::write::java_float(f);
    match text.parse::<f64>() {
        Ok(d) => Value::Num(Number::Float(d)),
        Err(_) => Value::Num(Number::Float(f as f64)),
    }
}

/// Whether an error is a Java `IllegalArgumentException` (or a subclass), which several query
/// parsers catch and rewrap.
pub fn is_illegal_argument(e: &EsError) -> bool {
    matches!(
        e.kind.as_ref(),
        "illegal_argument_exception" | "x_content_parse_exception" | "number_format_exception" | "pattern_syntax_exception"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(src: &str) -> Vec<(Token, Option<String>, (usize, usize))> {
        let v = crate::json::parse(src.as_bytes()).unwrap().unwrap();
        let mut p = XParser::new(&v, Some(src.as_bytes()), Vec::new());
        let mut out = Vec::new();
        while let Some(t) = p.next_token() {
            out.push((t, p.current_name().map(str::to_string), p.token_location()));
        }
        out
    }

    #[test]
    fn walks_tokens_with_locations() {
        let t = tokens(r#"{"a":[1,{"b":true}],"c":{}}"#);
        let kinds: Vec<Token> = t.iter().map(|x| x.0).collect();
        use Token::*;
        assert_eq!(
            kinds,
            vec![
                StartObject, FieldName, StartArray, Number, StartObject, FieldName, Boolean, EndObject, EndArray, FieldName,
                StartObject, EndObject, EndObject
            ]
        );
        assert_eq!(t[1].1.as_deref(), Some("a"));
        assert_eq!(t[1].2, (1, 2));
        assert_eq!(t[2].1.as_deref(), Some("a"));
        assert_eq!(t[3].1, None);
        assert_eq!(t[3].2, (1, 7));
        assert_eq!(t[5].2, (1, 10));
        assert_eq!(t[6].1.as_deref(), Some("b"));
        assert_eq!(t[7].2, (1, 18));
        assert_eq!(t[8].2, (1, 19));
        assert_eq!(t[9].1.as_deref(), Some("c"));
        assert_eq!(t[11].1.as_deref(), Some("c"));
    }

    #[test]
    fn skip_children_and_accessors() {
        let src = r#"{"x":{"y":[1,2]},"n":"12","f":"1.5","b":"true","t":true,"z":null}"#;
        let v = crate::json::parse(src.as_bytes()).unwrap().unwrap();
        let mut p = XParser::new(&v, Some(src.as_bytes()), Vec::new());
        p.next_token();
        p.next_token();
        assert_eq!(p.next_token(), Some(Token::StartObject));
        p.skip_children();
        assert_eq!(p.token(), Token::EndObject);
        assert_eq!(p.next_token(), Some(Token::FieldName));
        p.next_token();
        assert_eq!(p.int_value().unwrap(), 12);
        p.next_token();
        p.next_token();
        assert_eq!(p.float_value().unwrap(), 1.5);
        assert_eq!(p.int_value().unwrap(), 1);
        p.next_token();
        p.next_token();
        assert!(p.boolean_value().unwrap());
        p.next_token();
        p.next_token();
        let e = p.float_value().unwrap_err();
        assert_eq!(e.kind, "x_content_parse_exception");
        assert!(e.reason().starts_with("[1:56] Current token (VALUE_TRUE) not numeric"), "{}", e.reason());
        p.next_token();
        p.next_token();
        assert_eq!(p.text().unwrap_err().reason(), "Expected text at 1:61 but found VALUE_NULL");
        assert_eq!(p.text_or_null().unwrap(), None);
    }

    #[test]
    fn number_coercion_texts() {
        assert_eq!(java_parse_double("abc").unwrap_err().reason(), "For input string: \"abc\"");
        assert_eq!(java_parse_double("").unwrap_err().reason(), "empty String");
        assert_eq!(java_to_long("1.9").unwrap(), 1);
        assert_eq!(java_to_long("1e30").unwrap_err().reason(), "Value [1e30] is out of range for a long");
        assert_eq!(float_value(1.1).to_json(), "1.1");
        assert_eq!(float_value(2.0).to_json(), "2.0");
    }
}
