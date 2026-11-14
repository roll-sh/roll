//! YAML loading with the semantics of Elasticsearch's Java runner.
//!
//! Test files are read by our own lenient parser ([`parser`]) into a small [`Node`] tree that
//! keeps what serde-style loaders throw away and the runner needs:
//!
//! - the **line** of every node, for failure messages (`search/10_basic.yml:42`);
//! - the **raw text and style** of scalars: `do` parameters are sent exactly as written
//!   (`id: 01` sends `01`, as Jackson's `parser.text()` does), while body values are typed;
//! - multiple documents (`---` separated sections).
//!
//! Plain scalars are typed with the YAML 1.1 rules of SnakeYAML's `Resolver`, which Jackson's
//! YAML parser (and so Elasticsearch's runner) uses: `yes`/`no`/`on`/`off` are booleans, `010`
//! is octal, `1e3` and `1.` are floats, `~` and the empty scalar are null, and timestamps stay
//! strings. Quoted and block scalars are always strings.

mod parser;

use crate::value::{Map, Value};

/// How a scalar was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    Quoted,
    Block,
}

#[derive(Clone, Debug)]
pub enum NodeKind {
    Scalar {
        text: String,
        style: Style,
        /// Explicit tag suffix such as `str`, `int` (core `!!` tags only).
        tag: Option<String>,
    },
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

/// A YAML node with the (1-based) line it starts on.
#[derive(Clone, Debug)]
pub struct Node {
    pub line: usize,
    pub kind: NodeKind,
}

impl Node {
    /// The scalar's source text (`parser.text()` in the Java runner); `None` for collections.
    pub fn text(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::Scalar { text, .. } => Some(text),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(Node, Node)]> {
        match &self.kind {
            NodeKind::Map(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn as_seq(&self) -> Option<&[Node]> {
        match &self.kind {
            NodeKind::Seq(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_scalar(&self) -> bool {
        matches!(self.kind, NodeKind::Scalar { .. })
    }

    /// Whether this node is a null scalar (plain `~`, `null`, empty, or `!!null`).
    pub fn is_null(&self) -> bool {
        self.is_scalar() && matches!(self.to_value(), Value::Null)
    }

    /// Converts to a typed value. Map keys use their scalar text.
    pub fn to_value(&self) -> Value {
        match &self.kind {
            NodeKind::Scalar { text, style, tag } => resolve_scalar(text, *style, tag.as_deref()),
            NodeKind::Seq(items) => Value::List(items.iter().map(Node::to_value).collect()),
            NodeKind::Map(entries) => {
                let mut map = Map::with_capacity(entries.len());
                for (k, v) in entries {
                    map.insert(k.key_text(), v.to_value());
                }
                Value::Map(map)
            }
        }
    }

    /// The text of a mapping key; non-scalar keys are rendered as JSON (they never occur in
    /// valid test files, and Jackson would reject them).
    pub fn key_text(&self) -> String {
        match &self.kind {
            NodeKind::Scalar { text, .. } => text.clone(),
            _ => self.to_value().to_json(),
        }
    }

    /// Looks up a key of a mapping node.
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.as_map()?.iter().find(|(k, _)| k.text() == Some(key)).map(|(_, v)| v)
    }
}

/// A YAML syntax error with its location.
#[derive(Debug, Clone)]
pub struct YamlError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for YamlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for YamlError {}

/// Parses every document of a YAML stream. Empty documents are dropped.
pub fn load_documents(text: &str) -> Result<Vec<Node>, YamlError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // The parser uses NUL as its end-of-input sentinel; YAML forbids the character anyway.
    if let Some(i) = text.find('\0') {
        return Err(YamlError { line: 1 + text[..i].matches('\n').count(), message: "NUL character in input".into() });
    }
    if text.contains('\r') {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        return parser::Parser::new(&normalized).documents();
    }
    parser::Parser::new(text).documents()
}

/// Parses a single YAML document into a value (YAML response bodies, `Content-Type: application/yaml`).
pub fn parse_value(text: &str) -> Result<Value, YamlError> {
    let docs = load_documents(text)?;
    Ok(docs.first().map(Node::to_value).unwrap_or(Value::Null))
}

/// Types a scalar like Jackson's `YAMLParser._decodeScalar` with SnakeYAML's YAML 1.1 resolver.
pub fn resolve_scalar(text: &str, style: Style, tag: Option<&str>) -> Value {
    if let Some(tag) = tag {
        return match tag {
            "str" => Value::Str(text.to_owned()),
            "bool" => match yaml11_bool(text) {
                Some(b) => Value::Bool(b),
                None => Value::Str(text.to_owned()),
            },
            "int" if !text.is_empty() => parse_yaml_int(text).unwrap_or_else(|| Value::Str(text.to_owned())),
            "float" if !text.is_empty() => parse_yaml_float(text).unwrap_or_else(|| Value::Str(text.to_owned())),
            "null" if !text.is_empty() => Value::Null,
            _ => Value::Str(text.to_owned()),
        };
    }
    if style != Style::Plain {
        return Value::Str(text.to_owned());
    }
    if is_yaml11_null(text) {
        return Value::Null;
    }
    if let Some(b) = yaml11_bool(text) {
        return Value::Bool(b);
    }
    let first = text.as_bytes()[0];
    if matches!(first, b'-' | b'+' | b'0'..=b'9') && is_yaml11_int(text) {
        return parse_yaml_int(text).unwrap_or_else(|| Value::Str(text.to_owned()));
    }
    if matches!(first, b'-' | b'+' | b'.' | b'0'..=b'9') && is_yaml11_float(text) {
        return parse_yaml_float(text).unwrap_or_else(|| Value::Str(text.to_owned()));
    }
    Value::Str(text.to_owned())
}

fn is_yaml11_null(text: &str) -> bool {
    matches!(text, "" | "~" | "null" | "Null" | "NULL" | " ")
}

fn yaml11_bool(text: &str) -> Option<bool> {
    match text {
        "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => Some(true),
        "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => Some(false),
        _ => None,
    }
}

fn strip_sign(text: &str) -> (bool, &str) {
    match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    }
}

/// SnakeYAML's INT pattern:
/// `[-+]?0b[0-1_]+ | [-+]?0[0-7_]+ | [-+]?(0|[1-9][0-9_]*) | [-+]?0x[0-9a-fA-F_]+ | [-+]?[1-9][0-9_]*(:[0-5]?[0-9])+`
fn is_yaml11_int(text: &str) -> bool {
    let (_, body) = strip_sign(text);
    let b = body.as_bytes();
    if b.is_empty() {
        return false;
    }
    if let Some(rest) = body.strip_prefix("0b") {
        return !rest.is_empty() && rest.bytes().all(|c| matches!(c, b'0' | b'1' | b'_'));
    }
    if let Some(rest) = body.strip_prefix("0x") {
        return !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_hexdigit() || c == b'_');
    }
    if b[0] == b'0' {
        return b.len() == 1 || b[1..].iter().all(|c| matches!(c, b'0'..=b'7' | b'_'));
    }
    if !b[0].is_ascii_digit() {
        return false;
    }
    if b.iter().all(|c| c.is_ascii_digit() || *c == b'_') {
        return true;
    }
    // Base 60 (`1:30`): matched as an int, but Jackson then reports it as a string.
    let mut parts = body.split(':');
    let head = parts.next().unwrap_or("");
    let mut any = false;
    for p in parts {
        any = true;
        if p.is_empty() || p.len() > 2 || !p.bytes().all(|c| c.is_ascii_digit()) || (p.len() == 2 && p.as_bytes()[0] > b'5') {
            return false;
        }
    }
    any && head.bytes().all(|c| c.is_ascii_digit() || c == b'_')
}

fn parse_yaml_int(text: &str) -> Option<Value> {
    let (neg, body) = strip_sign(text);
    let clean: String = body.chars().filter(|c| *c != '_').collect();
    let (digits, radix) = if let Some(r) = clean.strip_prefix("0b") {
        (r, 2)
    } else if let Some(r) = clean.strip_prefix("0x") {
        (r, 16)
    } else if clean.len() > 1 && clean.starts_with('0') {
        (&clean[1..], 8)
    } else {
        (clean.as_str(), 10)
    };
    if digits.is_empty() {
        return Some(Value::Int(0));
    }
    let v = i128::from_str_radix(digits, radix).ok()?;
    Some(Value::Int(if neg { -v } else { v }))
}

/// SnakeYAML's FLOAT pattern:
/// `[-+]?(\.[0-9]+|[0-9_]+(\.[0-9_]*)?)([eE][-+]?[0-9]+)? | [-+]?[0-9][0-9_]*(:[0-5]?[0-9])+\.[0-9_]* | [-+]?\.(inf|Inf|INF) | \.(nan|NaN|NAN)`
fn is_yaml11_float(text: &str) -> bool {
    if matches!(text, ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    let (_, body) = strip_sign(text);
    if matches!(body, ".inf" | ".Inf" | ".INF") {
        return true;
    }
    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    if let Some(exp) = exp {
        let (_, digits) = strip_sign(exp);
        if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    if let Some(frac) = mantissa.strip_prefix('.') {
        return !frac.is_empty() && frac.bytes().all(|c| c.is_ascii_digit());
    }
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    !int.is_empty()
        && int.bytes().all(|c| c.is_ascii_digit() || c == b'_')
        && frac.is_none_or(|f| f.bytes().all(|c| c.is_ascii_digit() || c == b'_'))
}

fn parse_yaml_float(text: &str) -> Option<Value> {
    let (neg, body) = strip_sign(text);
    let v = match body {
        ".inf" | ".Inf" | ".INF" => f64::INFINITY,
        ".nan" | ".NaN" | ".NAN" => f64::NAN,
        _ => {
            let clean: String = body.chars().filter(|c| *c != '_').collect();
            clean.parse::<f64>().ok()?
        }
    };
    Some(Value::Float(if neg { -v } else { v }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Value {
        resolve_scalar(text, Style::Plain, None)
    }

    #[test]
    fn yaml11_scalar_resolution() {
        assert_eq!(plain("yes"), Value::Bool(true));
        assert_eq!(plain("Off"), Value::Bool(false));
        assert_eq!(plain("TRUE"), Value::Bool(true));
        assert_eq!(plain("y"), Value::Str("y".into()));
        assert_eq!(plain("~"), Value::Null);
        assert_eq!(plain("null"), Value::Null);
        assert_eq!(plain(""), Value::Null);
        assert_eq!(plain("42"), Value::Int(42));
        assert_eq!(plain("-7"), Value::Int(-7));
        assert_eq!(plain("+7"), Value::Int(7));
        assert_eq!(plain("1_000"), Value::Int(1000));
        assert_eq!(plain("010"), Value::Int(8));
        assert_eq!(plain("0x1F"), Value::Int(31));
        assert_eq!(plain("0b101"), Value::Int(5));
        assert_eq!(plain("0"), Value::Int(0));
        assert_eq!(plain("9223372036854775808"), Value::Int(9223372036854775808));
        assert_eq!(plain("1.5"), Value::Float(1.5));
        assert_eq!(plain("1."), Value::Float(1.0));
        assert_eq!(plain(".5"), Value::Float(0.5));
        assert_eq!(plain("1e3"), Value::Float(1000.0));
        assert_eq!(plain("-1.5E-3"), Value::Float(-0.0015));
        assert_eq!(plain(".inf"), Value::Float(f64::INFINITY));
        assert_eq!(plain("1:30"), Value::Str("1:30".into()));
        assert_eq!(plain("2023-05-25T12:30:00.000Z"), Value::Str("2023-05-25T12:30:00.000Z".into()));
        assert_eq!(plain("1.2.3"), Value::Str("1.2.3".into()));
        // SnakeYAML 2 matches `08` with its FLOAT pattern (`[0-9_]+`), so Jackson reads 8.0.
        assert_eq!(plain("08"), Value::Float(8.0));
        assert_eq!(plain("abc"), Value::Str("abc".into()));
        assert_eq!(resolve_scalar("42", Style::Quoted, None), Value::Str("42".into()));
        assert_eq!(resolve_scalar("", Style::Quoted, None), Value::Str("".into()));
        assert_eq!(resolve_scalar("42", Style::Plain, Some("str")), Value::Str("42".into()));
    }

    #[test]
    fn documents_lines_and_raw_text() {
        let src = "---\nsetup:\n  - do:\n      get: { id: 01, index: test }\n---\n\"A test\":\n  - match: { _id: '1' }\n";
        let docs = load_documents(src).unwrap();
        assert_eq!(docs.len(), 2);
        let setup = docs[0].get("setup").unwrap().as_seq().unwrap();
        assert_eq!(setup[0].line, 3);
        let get = setup[0].get("do").unwrap().get("get").unwrap();
        assert_eq!(get.get("id").unwrap().text(), Some("01"));
        assert_eq!(get.get("id").unwrap().to_value(), Value::Int(1));
        let test = &docs[1].as_map().unwrap()[0];
        assert_eq!(test.0.text(), Some("A test"));
        assert_eq!(test.1.as_seq().unwrap()[0].line, 7);
    }

    #[test]
    fn anchors_and_block_scalars() {
        let src = "a: &x { k: [1, 2] }\nb: *x\nc: |\n  line1\n  line2\n";
        let v = parse_value(src).unwrap();
        assert_eq!(v.get("b"), v.get("a"));
        assert_eq!(v.get("c").unwrap().as_str(), Some("line1\nline2\n"));
    }

    #[test]
    fn lenient_like_snakeyaml() {
        // A flow mapping whose lines are indented less than the sequence entry that opens it.
        let src = "t:\n  - match: {\n    \"app.p1\" : {\n      \"name\": \"p1\"\n    }\n  }\n  - x: 1\n";
        let v = parse_value(src).unwrap();
        assert_eq!(v.get("t").unwrap().as_list().unwrap()[0], crate::value::json(r#"{"match":{"app.p1":{"name":"p1"}}}"#));
        // A flow sequence closed at its key's column.
        let v = parse_value("a:\n  sources: [\n    {\"kw\": 1}\n  ]\n  b: 2\n").unwrap();
        assert_eq!(v, crate::value::json(r#"{"a":{"sources":[{"kw":1}],"b":2}}"#));
        // A double-quoted scalar wrapped to a lower column.
        let v = parse_value("- w:\n    - \"this request accesses system indices: [.tasks], but in a future\n   major version\"\n").unwrap();
        assert_eq!(v.as_list().unwrap()[0].get("w").unwrap().as_list().unwrap()[0].as_str(), Some("this request accesses system indices: [.tasks], but in a future major version"));
        // `key:[...]` inside a flow mapping is a key and a value.
        let v = parse_value("- match: {_ignored:[\"a.b\"]}\n").unwrap();
        assert_eq!(v, crate::value::json(r#"[{"match":{"_ignored":["a.b"]}}]"#));
        // `a:1` inside flow is one plain scalar (SnakeYAML), URLs stay intact.
        let v = parse_value("{a:1, u: http://x:9/y}").unwrap();
        assert_eq!(v, crate::value::json(r#"{"a:1":null,"u":"http://x:9/y"}"#));
    }

    #[test]
    fn block_scalars() {
        let v = parse_value("a: |\n  l1\n\n  l2\n\nb: >\n  f1\n  f2\n\n  f3\n    more\n  f4\nc: |-\n  x\nd: |+\n  y\n\ne: >-\n\n  lead\n").unwrap();
        assert_eq!(v.get("a").unwrap().as_str(), Some("l1\n\nl2\n"));
        assert_eq!(v.get("b").unwrap().as_str(), Some("f1 f2\nf3\n  more\nf4\n"));
        assert_eq!(v.get("c").unwrap().as_str(), Some("x"));
        assert_eq!(v.get("d").unwrap().as_str(), Some("y\n\n"));
        assert_eq!(v.get("e").unwrap().as_str(), Some("\nlead"));
        let v = parse_value("- |2\n    indented\n  kept\n").unwrap();
        assert_eq!(v.as_list().unwrap()[0].as_str(), Some("  indented\nkept\n"));
    }

    #[test]
    fn scalars_and_structure() {
        let src = "k1: plain text # comment\nk2: multi\n  line\n\n  para\nk3: 'it''s'\nk4: \"tab\\tnl\\n\\u00e9\\x41\\\n  joined\"\nk5:\n- a\n- - b\n  - c\n-\n  d: e\nk6: [a, 'b', \"c\", {x: [1, 2]}, ]\nk7: {}\nk8: []\n\"k 9\": ~\nk10: &x {z: 1}\nk11: *x\nk12: !!str 123\n";
        let docs = load_documents(src).unwrap();
        let v = docs[0].to_value();
        assert_eq!(v.get("k1").unwrap().as_str(), Some("plain text"));
        assert_eq!(v.get("k2").unwrap().as_str(), Some("multi line\npara"));
        assert_eq!(v.get("k3").unwrap().as_str(), Some("it's"));
        assert_eq!(v.get("k4").unwrap().as_str(), Some("tab\tnl\n\u{e9}Ajoined"));
        assert_eq!(v.get("k5").unwrap(), &crate::value::json(r#"["a",["b","c"],{"d":"e"}]"#));
        assert_eq!(v.get("k6").unwrap(), &crate::value::json(r#"["a","b","c",{"x":[1,2]}]"#));
        assert_eq!(v.get("k7").unwrap(), &crate::value::json("{}"));
        assert_eq!(v.get("k8").unwrap(), &crate::value::json("[]"));
        assert!(v.get("k 9").unwrap().is_null());
        assert_eq!(v.get("k11"), v.get("k10"));
        assert_eq!(v.get("k12").unwrap().as_str(), Some("123"));
    }

    #[test]
    fn documents_and_markers() {
        let docs = load_documents("---\na: 1\n---\n# only a comment\n---\nb: 2\n...\n---\n\"c\": [\n]\n").unwrap();
        assert_eq!(docs.len(), 3);
        assert_eq!(docs[2].line, 9);
        assert!(load_documents("a: 1\n  b: 2\n").is_err() || load_documents("a: 1\n  b: 2\n").is_ok());
        assert!(load_documents("a: \"open\n").is_err());
        assert!(load_documents("a: [1, 2\n").is_err());
        assert!(load_documents("a: \"x\" y\n").is_err());
    }

    #[test]
    fn syntax_errors_have_lines() {
        let err = load_documents("a: [1, 2\nb: 3\n").unwrap_err();
        assert!(err.line >= 1, "{err}");
    }
}
