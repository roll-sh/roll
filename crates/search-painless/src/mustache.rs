//! Mustache templates (`lang: mustache`): search templates and ingest templates.
//!
//! Elasticsearch renders templates with mustache.java plus its own extensions, and clients see
//! the quirks of both, so the parser below replays mustache.java's single-pass algorithm rather
//! than the Mustache spec: its standalone-line handling (a section that spans lines swallows the
//! newline after its opening tag and the whitespace before its closing tag, and the first line
//! inside a section drops its newline when it holds only whitespace and tags), its delimiter
//! changes, its error messages with `@[query-template:N]` line suffixes, and how text after a
//! tag belongs to that tag (which decides what `{{#toJson}}` accepts as "one identifier").
//!
//! Rendering follows `CustomReflectionObjectHandler`: maps resolve keys, lists and arrays act as
//! maps from index to element with a `size` key, dotted names resolve their first segment through
//! the scope stack and the rest inside it, values print with Java's `toString` (a list prints as
//! `{0=a, 1=b}` because Elasticsearch wraps collections in a map) and are escaped by the encoder
//! of the `content_type` option (JSON string escaping by default). The custom sections are
//! `{{#toJson}}name{{/toJson}}`, `{{#join}}name{{/join}}` (also with `delimiter='…'`) and
//! `{{#url}}…{{/url}}`.

use std::rc::Rc;

use crate::error::ScriptError;
use crate::types::cls;
use crate::value::{JMap, Obj, Value};

const FILE: &str = "query-template";

/// Default `mustache.max_output_size_bytes` (1mb).
pub const DEFAULT_MAX_OUTPUT: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoder {
    Json,
    Plain,
    Url,
}

#[derive(Debug)]
enum Kind {
    Write(String),
    Value { name: String, encoded: bool },
    Section { name: String, codes: Vec<Code> },
    Inverted { name: String, codes: Vec<Code> },
    ToJson { var: String },
    Join { var: String, delimiter: String, codes: Vec<Code> },
    Url { codes: Vec<Code> },
    Comment,
}

/// One mustache.java `Code`: what it renders plus the text that follows it.
#[derive(Debug)]
struct Code {
    kind: Kind,
    append: String,
}

/// A compiled template. Immutable and shareable across threads.
#[derive(Debug)]
pub struct Template {
    codes: Vec<Code>,
    encoder: Encoder,
    detect_missing_params: bool,
    source: String,
}

fn err(src: &str, msg: String) -> ScriptError {
    ScriptError {
        reason: msg.clone(),
        script_stack: Vec::new(),
        script: src.to_string(),
        lang: "mustache",
        position: None,
        cause_type: "mustache_exception".into(),
        cause_reason: Some(msg),
        cause_is_root: false,
        metadata: Vec::new(),
        status: 400,
        plain: false,
        cause_fields: Vec::new(),
    }
}

struct Reader<'a> {
    s: &'a [char],
    pos: usize,
    mark: usize,
}

impl Reader<'_> {
    fn read(&mut self) -> Option<char> {
        let c = self.s.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }
    fn mark(&mut self) {
        self.mark = self.pos;
    }
    fn reset(&mut self) {
        self.pos = self.mark;
    }
}

struct Parser<'a> {
    r: Reader<'a>,
    line: usize,
    sm: Vec<char>,
    em: Vec<char>,
    src: &'a str,
}

/// The code list a visitor builds: text is appended to the previous code when there is one.
struct Visitor {
    list: Vec<Code>,
}

impl Visitor {
    fn write(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.list.last_mut() {
            Some(c) => c.append.push_str(text),
            None => self.list.push(Code { kind: Kind::Write(text.to_string()), append: String::new() }),
        }
    }
    fn add(&mut self, kind: Kind) {
        self.list.push(Code { kind, append: String::new() });
    }
}

impl<'a> Parser<'a> {
    fn fail(&self, msg: &str, line: usize) -> ScriptError {
        err(self.src, format!("{msg} @[{FILE}:{line}]"))
    }

    /// `MustacheParser.compile`: parses until the closing tag of `tag` (or the end of input
    /// for the top level).
    fn compile(&mut self, tag: Option<&str>, mut start_of_line: bool) -> Result<Vec<Code>, ScriptError> {
        let mut saw_cr = false;
        let start_line = self.line;
        let mut mv = Visitor { list: Vec::new() };
        let mut only_ws = true;
        let mut iterable = self.line != 0;
        if self.line == 0 {
            self.line = 1;
        }
        let mut out = String::new();
        let mut n: usize = 0;
        while let Some(c) = self.r.read() {
            n += 1;
            if c == '\r' {
                saw_cr = true;
                continue;
            }
            if c == '\n' {
                self.line += 1;
                if !iterable || !only_ws {
                    if saw_cr {
                        out.push('\r');
                    }
                    out.push('\n');
                }
                mv.write(&out);
                out.clear();
                iterable = false;
                only_ws = true;
                start_of_line = true;
                n = 0;
                continue;
            }
            saw_cr = false;
            if c == self.sm[0] {
                self.r.mark();
                if self.sm.len() == 1 || self.r.read() == Some(self.sm[1]) {
                    let mut sb = String::new();
                    self.r.mark();
                    let mut cur = self.r.read();
                    n += 1;
                    let mut delimiter = cur == Some('=');
                    if delimiter {
                        sb.push('=');
                    } else {
                        self.r.reset();
                        n -= 1;
                    }
                    loop {
                        cur = self.r.read();
                        let Some(ch) = cur else { break };
                        n += 1;
                        self.r.mark();
                        if delimiter {
                            if ch == '=' {
                                delimiter = false;
                            } else {
                                sb.push(ch);
                            }
                            continue;
                        }
                        if ch == self.em[0] {
                            if self.em.len() > 1 {
                                if self.r.read() == Some(self.em[1]) {
                                    n += 1;
                                    break;
                                } else {
                                    self.r.reset();
                                    n -= 1;
                                }
                            } else {
                                break;
                            }
                        }
                        sb.push(ch);
                    }
                    let command = sb;
                    if command.is_empty() {
                        return Err(self.fail("Empty mustache", self.line));
                    }
                    let ch = command.chars().next().unwrap();
                    let variable = command[ch.len_utf8()..].trim().to_string();
                    match ch {
                        '#' | '^' | '<' | '?' | '$' => {
                            let old_sol = start_of_line;
                            start_of_line = start_of_line && only_ws;
                            let line = self.line;
                            let codes = self.compile(Some(&variable), start_of_line)?;
                            let lines = self.line - line;
                            if !only_ws || lines == 0 {
                                let _ = old_sol;
                                mv.write(&out);
                            }
                            out.clear();
                            let kind = match ch {
                                '^' => Kind::Inverted { name: variable.clone(), codes },
                                '<' => return Err(err(self.src, format!("Cannot expand '{variable}' because partial templates are not supported"))),
                                _ => self.section(variable.clone(), codes)?,
                            };
                            mv.add(kind);
                            iterable = lines != 0;
                        }
                        '/' => {
                            if !start_of_line || !only_ws {
                                mv.write(&out);
                            }
                            if tag != Some(variable.as_str()) {
                                let t = tag.unwrap_or("null");
                                let line = self.line;
                                return Err(self.fail(&format!("Mismatched start/end tags: {t} != {variable} in {FILE}:{line}"), line));
                            }
                            return Ok(mv.list);
                        }
                        '>' => {
                            return Err(err(self.src, format!("Cannot expand '{variable}' because partial templates are not supported")));
                        }
                        '{' => {
                            mv.write(&out);
                            out.clear();
                            let name = if self.em.len() < 2 || self.em[1] != '}' {
                                let mut v = variable.clone();
                                v.pop();
                                v
                            } else {
                                if self.r.read() != Some('}') {
                                    n += 1;
                                    let line = self.line;
                                    return Err(self.fail(&format!("Improperly closed variable: {variable} in {FILE}:{line}@{n}"), line));
                                }
                                variable.clone()
                            };
                            mv.add(Kind::Value { name, encoded: false });
                        }
                        '&' => {
                            mv.write(&out);
                            out.clear();
                            mv.add(Kind::Value { name: variable.clone(), encoded: false });
                        }
                        '%' => {
                            mv.write(&out);
                            out.clear();
                        }
                        '!' => {
                            mv.add(Kind::Comment);
                            mv.write(&out);
                            out.clear();
                        }
                        '=' => {
                            mv.write(&out);
                            out.clear();
                            let trimmed = command[1..].trim();
                            let split: Vec<&str> = trimmed.split_whitespace().collect();
                            if split.len() != 2 {
                                return Err(self.fail(&format!("Invalid delimiter string: {trimmed}"), self.line));
                            }
                            self.sm = split[0].chars().collect();
                            self.em = split[1].chars().collect();
                        }
                        _ => {
                            if cur.is_none() {
                                return Err(self.fail("Improperly closed variable", self.line));
                            }
                            mv.write(&out);
                            out.clear();
                            mv.add(Kind::Value { name: command.trim().to_string(), encoded: true });
                        }
                    }
                    start_of_line = false;
                    continue;
                } else {
                    self.r.reset();
                }
            }
            only_ws = only_ws && (c == ' ' || c == '\t' || c == '\r');
            out.push(c);
        }
        mv.write(&out);
        if let Some(t) = tag {
            return Err(self.fail(&format!("Failed to close '{t}' tag"), start_line));
        }
        Ok(mv.list)
    }

    /// `CustomMustacheVisitor.iterable`: the custom functions or a plain section.
    fn section(&self, variable: String, codes: Vec<Code>) -> Result<Kind, ScriptError> {
        if variable.eq_ignore_ascii_case("toJson") {
            let var = self.function_variable("toJson", &codes)?;
            return Ok(Kind::ToJson { var });
        }
        if variable.eq_ignore_ascii_case("join") {
            let var = self.function_variable("join", &codes)?;
            return Ok(Kind::Join { var, delimiter: ",".into(), codes });
        }
        if let Some(rest) = variable.strip_prefix("join delimiter='")
            && let Some(delim) = rest.strip_suffix('\'') {
                let var = self.function_variable("join", &codes)?;
                return Ok(Kind::Join { var, delimiter: delim.to_string(), codes });
            }
        if variable.eq_ignore_ascii_case("url") {
            return Ok(Kind::Url { codes });
        }
        Ok(Kind::Section { name: variable, codes })
    }

    /// `CustomCode.extractVariableName`: the body must be exactly one code; plain text is the
    /// variable name, anything else contributes its tag text (which never resolves).
    fn function_variable(&self, func: &str, codes: &[Code]) -> Result<String, ScriptError> {
        if codes.len() != 1 {
            return Err(err(self.src, format!("Mustache function [{func}] must contain one and only one identifier")));
        }
        Ok(match &codes[0].kind {
            Kind::Write(t) => format!("{t}{}", codes[0].append),
            Kind::Value { name, encoded: true } => format!("{{{{{name}}}}}"),
            Kind::Value { name, encoded: false } => format!("{{{{{{{name}}}}}}}"),
            _ => String::from("{{#}}"),
        })
    }
}

impl Template {
    /// Compiles a template. `options` are the script options (`content_type`,
    /// `detect_missing_params`).
    pub fn compile(src: &str, options: &[(String, String)]) -> Result<Template, ScriptError> {
        let mut encoder = Encoder::Json;
        let mut detect_missing_params = false;
        for (k, v) in options {
            match k.as_str() {
                "content_type" => {
                    encoder = match v.as_str() {
                        "application/json" | "application/json;charset=utf-8" | "application/json; charset=UTF-8" => Encoder::Json,
                        "text/plain" => Encoder::Plain,
                        "application/x-www-form-urlencoded" => Encoder::Url,
                        other => {
                            return Err(ScriptError::plain("illegal_argument_exception", format!("No encoder found for media type [{other}]")));
                        }
                    }
                }
                "detect_missing_params" => detect_missing_params = v.eq_ignore_ascii_case("true"),
                _ => {}
            }
        }
        let chars: Vec<char> = src.chars().collect();
        let mut p = Parser { r: Reader { s: &chars, pos: 0, mark: 0 }, line: 0, sm: vec!['{', '{'], em: vec!['}', '}'], src };
        let codes = p.compile(None, true)?;
        Ok(Template { codes, encoder, detect_missing_params, source: src.to_string() })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Renders with JSON params (converted like Elasticsearch parses them: `HashMap`s).
    pub fn render_json(&self, params: &serde_json::Value) -> Result<String, ScriptError> {
        let v = match params {
            serde_json::Value::Object(_) => Value::from_json(params, cls::HASH_MAP),
            _ => Value::Map(Rc::new(JMap::hash_map())),
        };
        self.render(&v, DEFAULT_MAX_OUTPUT)
    }

    /// Renders with a params value (a map), failing when the output exceeds `max_output`
    /// UTF-16 units (`mustache.max_output_size_bytes`).
    pub fn render(&self, params: &Value, max_output: usize) -> Result<String, ScriptError> {
        let mut r = Render { out: String::new(), units: 0, max: max_output, enc: self.encoder, detect: self.detect_missing_params, failed: None };
        let mut scopes: Vec<Value> = vec![params.clone()];
        r.codes(&self.codes, &mut scopes);
        if let Some(e) = r.failed {
            return Err(e);
        }
        Ok(r.out)
    }
}

struct Render {
    out: String,
    units: usize,
    max: usize,
    enc: Encoder,
    detect: bool,
    failed: Option<ScriptError>,
}

/// A lookup result: found (possibly null) or missing.
enum Found {
    Yes(Value),
    No,
}

impl Render {
    fn write(&mut self, s: &str) {
        if self.failed.is_some() {
            return;
        }
        let n = if s.is_ascii() { s.len() } else { s.encode_utf16().count() };
        if self.units + n > self.max {
            self.failed = Some(ScriptError::plain("parse_exception", "Mustache script result size limit exceeded".into()));
            return;
        }
        self.units += n;
        self.out.push_str(s);
    }

    fn encode(&mut self, s: &str) {
        match self.enc {
            Encoder::Plain => self.write(s),
            Encoder::Json => {
                let e = json_escape(s);
                self.write(&e);
            }
            Encoder::Url => {
                let e = url_encode(s);
                self.write(&e);
            }
        }
    }

    fn codes(&mut self, codes: &[Code], scopes: &mut Vec<Value>) {
        for c in codes {
            if self.failed.is_some() {
                return;
            }
            self.code(c, scopes);
            self.write(&c.append);
        }
    }

    fn code(&mut self, c: &Code, scopes: &mut Vec<Value>) {
        match &c.kind {
            Kind::Write(t) => self.write(t),
            Kind::Comment => {}
            Kind::Value { name, encoded } => match resolve(name, scopes) {
                Found::Yes(v) => {
                    if v.is_null() {
                        return;
                    }
                    let s = stringify(&v);
                    if *encoded {
                        self.encode(&s);
                    } else {
                        self.write(&s);
                    }
                }
                Found::No => {
                    if self.detect {
                        self.failed = Some(missing_param(name));
                    }
                }
            },
            Kind::Section { name, codes } => {
                let v = match resolve(name, scopes) {
                    Found::Yes(v) => v,
                    Found::No => return,
                };
                match &v {
                    Value::Null | Value::Bool(false) => {}
                    Value::Str(s) if s.as_str().is_empty() => {}
                    _ => match elements(&v) {
                        Some(items) => {
                            for it in items {
                                scopes.push(it);
                                self.codes(codes, scopes);
                                scopes.pop();
                            }
                        }
                        None => {
                            scopes.push(v);
                            self.codes(codes, scopes);
                            scopes.pop();
                        }
                    },
                }
            }
            Kind::Inverted { name, codes } => {
                let v = match resolve(name, scopes) {
                    Found::Yes(v) => v,
                    Found::No => Value::Null,
                };
                let falsey = match &v {
                    Value::Null | Value::Bool(false) => true,
                    Value::Bool(true) => false,
                    Value::Str(s) => s.as_str().is_empty(),
                    other => match elements(other) {
                        Some(items) => items.is_empty(),
                        None => false,
                    },
                };
                if falsey {
                    scopes.push(v);
                    self.codes(codes, scopes);
                    scopes.pop();
                }
            }
            Kind::ToJson { var } => {
                let v = match resolve(var, scopes) {
                    Found::Yes(v) => v,
                    Found::No => Value::Null,
                };
                if v.is_null() {
                    return;
                }
                let s = match &v {
                    Value::Map(_) => {
                        let mut s = String::new();
                        write_json(&v, &mut s);
                        s
                    }
                    other if elements(other).is_some() => {
                        let mut s = String::from("[");
                        for (i, x) in elements(other).unwrap().iter().enumerate() {
                            if i > 0 {
                                s.push(',');
                            }
                            write_json(x, &mut s);
                        }
                        s.push(']');
                        s
                    }
                    other => stringify(other),
                };
                self.write(&s);
            }
            Kind::Join { var, delimiter, codes } => {
                let v = match resolve(var, scopes) {
                    Found::Yes(v) => v,
                    Found::No => Value::Null,
                };
                match elements(&v) {
                    Some(items) if !matches!(v, Value::Map(_)) => {
                        let parts: Vec<String> = items.iter().map(|x| x.to_java_string()).collect();
                        let s = parts.join(delimiter);
                        self.write(&s);
                    }
                    _ => {
                        // not iterable: the rendered body comes out unchanged
                        let mut sub = Render { out: String::new(), units: 0, max: self.max, enc: self.enc, detect: self.detect, failed: None };
                        sub.codes(codes, scopes);
                        if let Some(e) = sub.failed {
                            self.failed = Some(e);
                            return;
                        }
                        let s = sub.out;
                        self.write(&s);
                    }
                }
            }
            Kind::Url { codes } => {
                for code in codes {
                    let mut sub = Render { out: String::new(), units: 0, max: self.max, enc: self.enc, detect: self.detect, failed: None };
                    sub.code(code, scopes);
                    sub.write(&code.append);
                    if let Some(e) = sub.failed {
                        self.failed = Some(e);
                        return;
                    }
                    let e = url_encode(&sub.out);
                    self.write(&e);
                }
            }
        }
    }
}

fn missing_param(name: &str) -> ScriptError {
    let reason = "Error running com.github.mustachejava.codes.DefaultMustache".to_string();
    ScriptError {
        reason: reason.clone(),
        script_stack: Vec::new(),
        script: String::new(),
        lang: "mustache",
        position: None,
        cause_type: "mustache_invalid_parameter_exception".into(),
        cause_reason: Some(format!("Parameter [{name}] is missing")),
        cause_is_root: false,
        metadata: Vec::new(),
        status: 400,
        plain: false,
        cause_fields: Vec::new(),
    }
    .general(reason)
}

impl ScriptError {
    /// Elasticsearch's `GeneralScriptException` wrapper.
    fn general(mut self, reason: String) -> ScriptError {
        self.plain = true;
        self.reason = reason;
        self
    }
}

/// Elements of a value mustache iterates (collections and arrays; `None` otherwise).
fn elements(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::List(l) => Some(l.v.borrow().clone()),
        Value::Set(s) => Some(s.keys()),
        Value::Obj(o) => match &**o {
            Obj::Array(a) => Some(a.data.borrow().to_values()),
            Obj::MapView { .. } => v.set_snapshot().or_else(|| match &**o {
                Obj::MapView { map, .. } => Some(map.values()),
                _ => None,
            }),
            _ => v.list_snapshot(),
        },
        _ => None,
    }
}

/// `CustomReflectionObjectHandler.stringify` after `coerce`.
fn stringify(v: &Value) -> String {
    match elements(v) {
        Some(items) => {
            let mut s = String::from("{");
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&i.to_string());
                s.push('=');
                x.write_java_string(&mut s, 1);
            }
            s.push('}');
            s
        }
        None => v.to_java_string(),
    }
}

/// Looks a key up in one (coerced) scope object.
fn lookup(scope: &Value, key: &str) -> Found {
    match scope {
        Value::Map(m) => {
            let k = Value::str(key);
            if m.contains(&k) {
                Found::Yes(m.get(&k).unwrap_or(Value::Null))
            } else {
                Found::No
            }
        }
        other => match elements(other) {
            Some(items) => {
                let got = if key == "size" {
                    Some(Value::Int(items.len() as i32))
                } else {
                    match crate::java::parse_int(key, 10) {
                        Some(i) if i >= 0 && (i as usize) < items.len() => Some(items[i as usize].clone()),
                        _ => None,
                    }
                };
                match got {
                    Some(v) if !v.is_null() => Found::Yes(v),
                    _ => Found::No,
                }
            }
            None => Found::No,
        },
    }
}

/// `ReflectionObjectHandler.find`: the first segment of a dotted name is looked up through the
/// scopes (innermost first); the remaining segments only inside what it found.
fn resolve(name: &str, scopes: &[Value]) -> Found {
    if name == "." {
        return Found::Yes(scopes.last().cloned().unwrap_or(Value::Null));
    }
    let mut segs = name.split('.');
    let first = segs.next().unwrap_or("");
    let rest: Vec<&str> = segs.collect();
    for scope in scopes.iter().rev() {
        if let Found::Yes(mut v) = lookup(scope, first) {
            for s in &rest {
                match lookup(&v, s) {
                    Found::Yes(x) => v = x,
                    Found::No => return Found::No,
                }
            }
            return Found::Yes(v);
        }
    }
    Found::No
}

/// Jackson's `JsonStringEncoder.quoteAsString`.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `URLEncoder.encode(s, UTF_8)`.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => out.push(b as char),
            b' ' => out.push('+'),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// XContent JSON of a value (maps in iteration order, doubles as Java prints them).
fn write_json(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Byte(_) | Value::Short(_) | Value::Int(_) | Value::Long(_) => out.push_str(&v.to_java_string()),
        Value::Float(f) => {
            if f.is_finite() {
                out.push_str(&crate::java::fmt_float(*f));
            } else {
                out.push_str(&format!("\"{}\"", crate::java::fmt_float(*f)));
            }
        }
        Value::Double(d) => {
            if d.is_finite() {
                out.push_str(&crate::java::fmt_double(*d));
            } else {
                out.push_str(&format!("\"{}\"", crate::java::fmt_double(*d)));
            }
        }
        Value::Map(m) => {
            out.push('{');
            for (i, (k, x)) in m.entries().iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&json_escape(&k.to_java_string()));
                out.push_str("\":");
                write_json(x, out);
            }
            out.push('}');
        }
        other => match elements(other) {
            Some(items) => {
                out.push('[');
                for (i, x) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json(x, out);
                }
                out.push(']');
            }
            None => {
                out.push('"');
                out.push_str(&json_escape(&other.to_java_string()));
                out.push('"');
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r(t: &str, p: serde_json::Value) -> String {
        Template::compile(t, &[]).unwrap().render_json(&p).unwrap()
    }

    fn e(t: &str) -> String {
        Template::compile(t, &[]).unwrap_err().cause_reason.unwrap()
    }

    // Expected outputs captured from Elasticsearch 9.5.5 (`_render/template`).
    #[test]
    fn values_and_escaping() {
        assert_eq!(r("@@{{x}}", json!({"x": "a\"b\\c\n\t\u{1}\u{1f}/<"})), "@@a\\\"b\\\\c\\n\\t\\u0001\\u001F/<");
        assert_eq!(r("{{x}}", json!({"x": [1, 2, "a"]})), "{0=1, 1=2, 2=a}");
        assert_eq!(r("{{x}}", json!({"x": {"k": 1, "j": [1]}})), "{j=[1], k=1}");
        assert_eq!(r("{{x}}", json!({"x": 1.0})), "1.0");
        assert_eq!(r("{{x}}", json!({"x": 1e300})), "1.0E300");
        assert_eq!(r("{{x}}", json!({"x": 1.5e-7})), "1.5E-7");
        assert_eq!(r("{{x}}|{{y}}", json!({"x": true})), "true|");
        assert_eq!(r("{{x}}", json!({"x": null})), "");
        assert_eq!(r("{{{x}}}|{{&x}}", json!({"x": "q\"<>&"})), "q\"<>&|q\"<>&");
        assert_eq!(r("{{ x }}", json!({"x": 1})), "1");
    }

    #[test]
    fn sections() {
        assert_eq!(r("{{#x}}[{{.}}]{{/x}}", json!({"x": 0})), "[0]");
        assert_eq!(r("{{#x}}[{{.}}]{{/x}}", json!({"x": ""})), "");
        assert_eq!(r("{{#x}}[{{.}}]{{/x}}", json!({"x": "s"})), "[s]");
        assert_eq!(r("{{#x}}[{{k}}]{{/x}}", json!({"x": {"k": 5}})), "[5]");
        assert_eq!(r("{{^x}}none{{/x}}", json!({"x": []})), "none");
        assert_eq!(r("{{^x}}none{{/x}}", json!({"x": 0})), "");
        assert_eq!(r("{{#x}}a{{/x}}{{^x}}b{{/x}}", json!({"x": null})), "b");
        assert_eq!(r("{{#x}}a{{/x}}{{^x}}b{{/x}}", json!({"x": false})), "b");
        assert_eq!(r("{{#x}}<{{.}}>{{/x}}", json!({"x": [[1, 2], [3]]})), "<{0=1, 1=2}><{0=3}>");
        assert_eq!(r("{{#x}}{{a}}-{{b}};{{/x}}", json!({"x": [{"a": 1}, {"a": 2, "b": 3}], "b": "outer"})), "1-outer;2-3;");
        assert_eq!(r("{{#x}}{{#y}}in{{/y}}{{/x}}", json!({"x": [1, 2], "y": true})), "inin");
    }

    #[test]
    fn lookups() {
        assert_eq!(r("{{x.size}} {{x.1}} {{x.length}}", json!({"x": [5, 6]})), "2 6 ");
        assert_eq!(r("{{x.0.a}}|{{x.5}}|{{x.-1}}", json!({"x": [{"a": 7}]})), "7||");
        assert_eq!(r("{{#x}}[{{a.b}}]{{/x}}", json!({"x": {"a": {}}, "a": {"b": 1}})), "[]");
        assert_eq!(r("{{#x}}[{{a.b}}]{{/x}}", json!({"x": {"c": {}}, "a": {"b": 1}})), "[1]");
        assert_eq!(r("{{#x}}[{{y}}]{{/x}}", json!({"x": {"y": null}, "y": 2})), "[]");
        assert_eq!(r("{{#x}}[{{.}}]{{/x}}", json!({"x": {"y": 1}})), "[{y=1}]");
    }

    #[test]
    fn functions() {
        assert_eq!(r("@@{{#toJson}}x{{/toJson}}", json!({"x": {"b": [1, 2.5, "s", null, true, {"c": "d\""}], "a": 1}})), "@@{\"a\":1,\"b\":[1,2.5,\"s\",null,true,{\"c\":\"d\\\"\"}]}");
        assert_eq!(r("{{#toJson}}x{{/toJson}}", json!({"x": "str"})), "str");
        assert_eq!(r("{{#toJson}}x.y{{/toJson}}", json!({"x": {"y": [1]}})), "[1]");
        assert_eq!(r("[{{#toJson}}missing{{/toJson}}]", json!({})), "[]");
        assert_eq!(r("[{{#toJson}}x{{/toJson}}]", json!({"x": [1.0, 1e300, -0.0, 12345678901234567i64]})), "[[1.0,1.0E300,-0.0,12345678901234567]]");
        assert_eq!(r("{{#join}}x{{/join}}", json!({"x": [1, 2.5, "s"]})), "1,2.5,s");
        assert_eq!(r("{{#join delimiter='||'}}x{{/join delimiter='||'}}", json!({"x": [1, 2, 3]})), "1||2||3");
        assert_eq!(r("[{{#join}}x{{/join}}]", json!({"x": "abc"})), "[x]");
        assert_eq!(r("[{{#join}}x{{/join}}]", json!({"x": []})), "[]");
        assert_eq!(r("[{{#join}}x{{/join}}]", json!({"x": [[1, 2], {"a": 1}]})), "[[1, 2],{a=1}]");
        assert_eq!(r("[{{#join}}x{{/join}}]", json!({"x": ["a\"b", "c\nd"]})), "[a\"b,c\nd]");
        assert_eq!(r("{{#url}}{{x}}{{/url}}", json!({"x": "a b&c/é~*"})), "a+b%26c%2F%C3%A9%7E*");
        assert_eq!(r("[{{#url}}{{x}}+{{/url}}]", json!({"x": "a\"b"})), "[a%5C%22b%2B]");
    }

    #[test]
    fn whitespace_like_mustache_java() {
        assert_eq!(r("@@\n  {{#x}}\n  item\n  {{/x}}\n end", json!({"x": true})), "@@\n  item\n end");
        assert_eq!(r("a {{#x}} b {{/x}} c\n  {{^x}}\nno\n  {{/x}}\nend", json!({"x": false})), "a  c\nno\nend");
        assert_eq!(r("@@ {{#x}}\n{{.}}\n {{/x}}\n", json!({"x": [1, 2]})), "@@ 1\n2\n\n");
        assert_eq!(r("@@\r\n{{#x}}\r\n{{.}}\r\n{{/x}}\r\n", json!({"x": [1, 2]})), "@@\r\n1\r\n2\r\n");
        assert_eq!(r("@@ {{! c }} \nA\n {{=| |=}} \n|x|", json!({"x": [1, 2]})), "@@  \nA\n  \n{0=1, 1=2}");
        assert_eq!(r("@@{{=<% %>=}}<%x%>{{x}}", json!({"x": 1})), "@@1{{x}}");
        assert_eq!(r("@@{{! comment }}a", json!({})), "@@a");
    }

    #[test]
    fn compile_errors() {
        assert_eq!(e("{{#x}}a"), "Failed to close 'x' tag @[query-template:1]");
        assert_eq!(e("a\n{{#x}}\nb\nc"), "Failed to close 'x' tag @[query-template:2]");
        assert_eq!(e("{{x"), "Improperly closed variable @[query-template:1]");
        assert_eq!(e("a\n\n{{x\nb"), "Improperly closed variable @[query-template:3]");
        assert_eq!(e("{{#toJson}}{{a}}{{b}}{{/toJson}}"), "Mustache function [toJson] must contain one and only one identifier");
        assert_eq!(e("{{#x}}a{{/y}}"), "Mismatched start/end tags: x != y in query-template:1 @[query-template:1]");
        assert_eq!(e("a\n{{#x}}\nb\n{{/y}}\nc"), "Mismatched start/end tags: x != y in query-template:4 @[query-template:4]");
        assert_eq!(e("{{/x}}"), "Mismatched start/end tags: null != x in query-template:1 @[query-template:1]");
        assert_eq!(e("{{>part}}"), "Cannot expand 'part' because partial templates are not supported");
        assert_eq!(e("{{=a=}}"), "Invalid delimiter string: a @[query-template:1]");
        assert_eq!(e("{{}}"), "Empty mustache @[query-template:1]");
        assert_eq!(e("a\n{{#x}}\n{{}}"), "Empty mustache @[query-template:3]");
        assert_eq!(e("{{{x}}"), "Improperly closed variable: x in query-template:1@6 @[query-template:1]");
        assert_eq!(e("{{{x}"), "Improperly closed variable: x} in query-template:1@4 @[query-template:1]");
        assert_eq!(e("{{#x}}{{{y}}}"), "Failed to close 'x' tag @[query-template:1]");
        assert_eq!(e("{{=<% %>=}}<%#x%><%/y%>"), "Mismatched start/end tags: x != y in query-template:1 @[query-template:1]");
        // `{{&x` at the end of input is accepted by mustache.java
        assert_eq!(r("{{&x", json!({"x": 1})), "1");
        let se = Template::compile("{{#x}}a", &[]).unwrap_err();
        assert_eq!(se.lang, "mustache");
        assert!(se.position.is_none());
    }

    #[test]
    fn encoders() {
        let t = Template::compile("{{x}}", &[("content_type".into(), "text/plain".into())]).unwrap();
        assert_eq!(t.render_json(&json!({"x": "a\"é"})).unwrap(), "a\"é");
        let t = Template::compile("{{x}}", &[("content_type".into(), "application/x-www-form-urlencoded".into())]).unwrap();
        assert_eq!(t.render_json(&json!({"x": "a b"})).unwrap(), "a+b");
        let e = Template::compile("{{x}}", &[("content_type".into(), "text/xml".into())]).unwrap_err();
        assert_eq!(e.cause_reason.unwrap(), "No encoder found for media type [text/xml]");
    }

    #[test]
    fn output_limit() {
        let t = Template::compile("{{x}}{{x}}", &[]).unwrap();
        let p = Value::from_json(&json!({"x": "abc"}), cls::HASH_MAP);
        assert_eq!(t.render(&p, 6).unwrap(), "abcabc");
        let e = t.render(&p, 5).unwrap_err();
        assert_eq!(e.cause_type, "parse_exception");
    }
}
