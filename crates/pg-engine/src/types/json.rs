//! JSON parsing with Postgres's error messages, and the `jsonb` value: numbers are `numeric`,
//! object keys are unique and sorted (shorter first, then bytewise), output is normalised.

use super::numeric::Numeric;
use crate::error::{Error, Result, code};
use std::cmp::Ordering;
use std::fmt::{self, Write};

#[derive(Clone, Debug)]
pub enum Jsonb {
    Null,
    Bool(bool),
    Number(Numeric),
    String(String),
    Array(Vec<Jsonb>),
    /// Sorted by `key_cmp`, keys unique.
    Object(Vec<(String, Jsonb)>),
}

/// jsonb key order: shorter keys first, then bytewise.
pub fn key_cmp(a: &str, b: &str) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

impl Jsonb {
    pub fn object(mut pairs: Vec<(String, Jsonb)>) -> Jsonb {
        // Last duplicate wins: stable sort, then keep the last of each run.
        pairs.sort_by(|a, b| key_cmp(&a.0, &b.0));
        let mut out: Vec<(String, Jsonb)> = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            if let Some(last) = out.last_mut()
                && last.0 == k
            {
                last.1 = v;
                continue;
            }
            out.push((k, v));
        }
        Jsonb::Object(out)
    }

    pub fn get(&self, key: &str) -> Option<&Jsonb> {
        match self {
            Jsonb::Object(pairs) => pairs
                .binary_search_by(|(k, _)| key_cmp(k, key))
                .ok()
                .map(|i| &pairs[i].1),
            _ => None,
        }
    }

    pub fn index(&self, i: i64) -> Option<&Jsonb> {
        match self {
            Jsonb::Array(items) => {
                let len = items.len() as i64;
                let idx = if i < 0 { len + i } else { i };
                if idx < 0 {
                    None
                } else {
                    items.get(idx as usize)
                }
            }
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Jsonb::Null => "null",
            Jsonb::Bool(_) => "boolean",
            Jsonb::Number(_) => "number",
            Jsonb::String(_) => "string",
            Jsonb::Array(_) => "array",
            Jsonb::Object(_) => "object",
        }
    }

    /// Text of a scalar as `->>` returns it (strings unquoted, null → None).
    pub fn as_text(&self) -> Option<String> {
        match self {
            Jsonb::Null => None,
            Jsonb::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }

    /// `@>` containment.
    pub fn contains(&self, other: &Jsonb) -> bool {
        match (self, other) {
            (Jsonb::Object(a), Jsonb::Object(b)) => {
                b.iter().all(|(k, bv)| match self.get(k) {
                    Some(av) => match (av, bv) {
                        (Jsonb::Array(_) | Jsonb::Object(_), _) => av.contains(bv),
                        _ => av == bv,
                    },
                    None => false,
                }) && (!a.is_empty() || b.is_empty())
            }
            (Jsonb::Array(a), Jsonb::Array(b)) => b.iter().all(|bv| {
                a.iter().any(|av| match (av, bv) {
                    (Jsonb::Array(_), Jsonb::Array(_)) | (Jsonb::Object(_), Jsonb::Object(_)) => {
                        av.contains(bv)
                    }
                    _ => av == bv,
                })
            }),
            // A top-level array contains a scalar it holds as an element.
            (Jsonb::Array(a), scalar) if !matches!(scalar, Jsonb::Array(_) | Jsonb::Object(_)) => {
                a.iter().any(|av| av == scalar)
            }
            (a, b) => a == b,
        }
    }
}

impl PartialEq for Jsonb {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Jsonb {}

impl PartialOrd for Jsonb {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Jsonb {
    fn cmp(&self, other: &Self) -> Ordering {
        compare(self, other).cmp(&0)
    }
}

/// varstr_cmp in the C collation: the first differing bytes' difference, else the shorter
/// string first.
pub fn varstr_cmp(a: &[u8], b: &[u8]) -> i32 {
    match a.iter().zip(b).find(|(x, y)| x != y) {
        Some((x, y)) => *x as i32 - *y as i32,
        None => match a.len().cmp(&b.len()) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        },
    }
}

/// compareJsonbContainers: Postgres's jsonb order, as jsonb_cmp returns it. A top-level
/// scalar is stored as a one-element "raw scalar" array, so an empty top-level array sorts
/// below every scalar and a longer one above.
pub fn compare(a: &Jsonb, b: &Jsonb) -> i32 {
    let scalar = |j: &Jsonb| !matches!(j, Jsonb::Array(_) | Jsonb::Object(_));
    match (a, b) {
        (a, Jsonb::Array(y)) if scalar(a) => {
            if y.is_empty() {
                1
            } else {
                -1
            }
        }
        (Jsonb::Array(x), b) if scalar(b) => {
            if x.is_empty() {
                -1
            } else {
                1
            }
        }
        _ => compare_inner(a, b),
    }
}

fn compare_inner(a: &Jsonb, b: &Jsonb) -> i32 {
    // The types' order: null, string, number, boolean, array, object.
    let rank = |j: &Jsonb| match j {
        Jsonb::Null => 0,
        Jsonb::String(_) => 1,
        Jsonb::Number(_) => 2,
        Jsonb::Bool(_) => 3,
        Jsonb::Array(_) => 0x10,
        Jsonb::Object(_) => 0x11,
    };
    let sign = |o: Ordering| o as i32;
    match (a, b) {
        (Jsonb::Array(x), Jsonb::Array(y)) => {
            if x.len() != y.len() {
                return if x.len() > y.len() { 1 } else { -1 };
            }
            x.iter()
                .zip(y)
                .map(|(p, q)| compare_inner(p, q))
                .find(|c| *c != 0)
                .unwrap_or(0)
        }
        (Jsonb::Object(x), Jsonb::Object(y)) => {
            if x.len() != y.len() {
                return if x.len() > y.len() { 1 } else { -1 };
            }
            for ((ka, va), (kb, vb)) in x.iter().zip(y) {
                let c = varstr_cmp(ka.as_bytes(), kb.as_bytes());
                if c != 0 {
                    return c;
                }
                let c = compare_inner(va, vb);
                if c != 0 {
                    return c;
                }
            }
            0
        }
        (Jsonb::Null, Jsonb::Null) => 0,
        (Jsonb::Bool(p), Jsonb::Bool(q)) => sign(p.cmp(q)),
        (Jsonb::Number(p), Jsonb::Number(q)) => sign(p.cmp(q)),
        (Jsonb::String(p), Jsonb::String(q)) => varstr_cmp(p.as_bytes(), q.as_bytes()),
        _ => {
            if rank(a) > rank(b) {
                1
            } else {
                -1
            }
        }
    }
}

pub fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl Jsonb {
    pub fn write(&self, out: &mut String) {
        match self {
            Jsonb::Null => out.push_str("null"),
            Jsonb::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Jsonb::Number(n) => {
                let _ = write!(out, "{n}");
            }
            Jsonb::String(s) => write_json_string(out, s),
            Jsonb::Array(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Jsonb::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_json_string(out, k);
                    out.push_str(": ");
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    /// `jsonb_pretty` output.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write_pretty(&self, out: &mut String, depth: usize) {
        let indent = |out: &mut String, d: usize| {
            for _ in 0..d {
                out.push_str("    ");
            }
        };
        match self {
            Jsonb::Array(items) => {
                out.push_str("[\n");
                for (i, v) in items.iter().enumerate() {
                    indent(out, depth + 1);
                    v.write_pretty(out, depth + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                indent(out, depth);
                out.push(']');
            }
            Jsonb::Object(pairs) => {
                out.push_str("{\n");
                for (i, (k, v)) in pairs.iter().enumerate() {
                    indent(out, depth + 1);
                    write_json_string(out, k);
                    out.push_str(": ");
                    v.write_pretty(out, depth + 1);
                    if i + 1 < pairs.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                indent(out, depth);
                out.push('}');
            }
            other => other.write(out),
        }
    }
}

impl fmt::Display for Jsonb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write(&mut s);
        f.write_str(&s)
    }
}

// ── parser ──

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
    for_jsonb: bool,
    /// Some object had a key twice.
    dup: bool,
    depth: usize,
}

/// A syntax error in JSON `input` whose offending token ends at byte `end` (`eof`: the input
/// ended), with Postgres' context line.
fn json_error(input: &str, end: usize, eof: bool, detail: String) -> Error {
    Error::new(
        code::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type json",
    )
    .detail(detail)
    .context(json_context(input, end, eof))
}

/// `report_json_context`: the line up to the offending token's end (its last 50 bytes or so).
fn json_context(s: &str, end: usize, eof: bool) -> String {
    let mut end = end.min(s.len());
    while !s.is_char_boundary(end) {
        end += 1;
    }
    let line_start = s[..end].rfind('\n').map_or(0, |i| i + 1);
    let line = s[..line_start].matches('\n').count() + 1;
    let mut start = line_start;
    while end - start >= 50 {
        start += s[start..].chars().next().map_or(1, char::len_utf8);
    }
    if start - line_start <= 3 {
        start = line_start;
    }
    let prefix = if start > line_start { "..." } else { "" };
    let suffix = if !eof && end < s.len() && !matches!(s.as_bytes()[end], b'\n' | b'\r') {
        "..."
    } else {
        ""
    };
    format!("JSON data, line {line}: {prefix}{}{suffix}", &s[start..end])
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn token_at(&self, start: usize) -> String {
        // The offending token: a string, a run of alphanumerics, or one character.
        let rest = &self.s[start..];
        if rest.starts_with('"') {
            let mut esc = false;
            for (i, c) in rest.char_indices().skip(1) {
                match c {
                    '\\' if !esc => esc = true,
                    '"' if !esc => return rest[..=i].to_string(),
                    _ => esc = false,
                }
            }
            return rest.to_string();
        }
        let mut end = 0;
        for (i, c) in rest.char_indices() {
            // JSON_ALPHANUMERIC_CHAR: ASCII letters, digits, `_` and any non-ASCII character.
            if c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii() {
                end = i + c.len_utf8();
            } else {
                if i == 0 {
                    end = c.len_utf8();
                }
                break;
            }
        }
        rest[..end].to_string()
    }

    fn invalid_token(&self, start: usize) -> Error {
        let tok = self.token_at(start);
        json_error(
            self.s,
            start + tok.len(),
            false,
            format!("Token \"{tok}\" is invalid."),
        )
    }

    fn expected(&self, what: &str) -> Error {
        if self.pos >= self.b.len() {
            return json_error(
                self.s,
                self.b.len(),
                true,
                "The input string ended unexpectedly.".into(),
            );
        }
        let tok = self.token_at(self.pos);
        // A word that isn't a JSON literal is a bad token before it is an unexpected one, and
        // so is a character no token starts with.
        if tok.starts_with(|c: char| c.is_alphabetic() || c == '_')
            && !matches!(tok.as_str(), "true" | "false" | "null")
            || !tok.starts_with(|c: char| {
                c.is_alphanumeric() || matches!(c, '{' | '}' | '[' | ']' | ',' | ':' | '"' | '-' | '_')
            })
        {
            return self.invalid_token(self.pos);
        }
        json_error(
            self.s,
            self.pos + tok.len(),
            false,
            format!("Expected {what}, but found \"{tok}\"."),
        )
    }

    fn value(&mut self) -> Result<Jsonb> {
        self.ws();
        if self.pos >= self.b.len() {
            return Err(json_error(
                self.s,
                self.b.len(),
                true,
                "The input string ended unexpectedly.".into(),
            ));
        }
        // Deep nesting fails like Postgres's check_stack_depth instead of overflowing the
        // thread's stack (frames are much bigger in debug builds).
        crate::stack::check()?;
        self.depth += 1;
        if self.depth > 6400 {
            return Err(crate::stack::depth_error());
        }
        let v = match self.b[self.pos] {
            b'{' => {
                self.pos += 1;
                let mut pairs = Vec::new();
                self.ws();
                if self.b.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                } else {
                    let mut first = true;
                    loop {
                        self.ws();
                        if self.b.get(self.pos) != Some(&b'"') {
                            return Err(self.expected(if first {
                                "string or \"}\""
                            } else {
                                "string"
                            }));
                        }
                        first = false;
                        let k = self.string()?;
                        self.ws();
                        if self.b.get(self.pos) != Some(&b':') {
                            return Err(self.expected("\":\""));
                        }
                        self.pos += 1;
                        let v = self.value()?;
                        pairs.push((k, v));
                        self.ws();
                        match self.b.get(self.pos) {
                            Some(b',') => self.pos += 1,
                            Some(b'}') => {
                                self.pos += 1;
                                break;
                            }
                            _ => return Err(self.expected("\",\" or \"}\"")),
                        }
                    }
                }
                let n = pairs.len();
                let o = Jsonb::object(pairs);
                if matches!(&o, Jsonb::Object(p) if p.len() < n) {
                    self.dup = true;
                }
                o
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                self.ws();
                if self.b.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                } else {
                    loop {
                        items.push(self.value()?);
                        self.ws();
                        match self.b.get(self.pos) {
                            Some(b',') => self.pos += 1,
                            Some(b']') => {
                                self.pos += 1;
                                break;
                            }
                            _ => return Err(self.expected("\",\" or \"]\"")),
                        }
                    }
                }
                Jsonb::Array(items)
            }
            b'"' => Jsonb::String(self.string()?),
            b't' if self.s[self.pos..].starts_with("true") && !self.ident_continues(4) => {
                self.pos += 4;
                Jsonb::Bool(true)
            }
            b'f' if self.s[self.pos..].starts_with("false") && !self.ident_continues(5) => {
                self.pos += 5;
                Jsonb::Bool(false)
            }
            b'n' if self.s[self.pos..].starts_with("null") && !self.ident_continues(4) => {
                self.pos += 4;
                Jsonb::Null
            }
            b'-' | b'0'..=b'9' => self.number()?,
            b'}' | b']' | b':' | b',' => return Err(self.expected("JSON value")),
            _ => return Err(self.invalid_token(self.pos)),
        };
        self.depth -= 1;
        Ok(v)
    }

    fn ident_continues(&self, len: usize) -> bool {
        self.b
            .get(self.pos + len)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
    }

    /// A malformed number from `start`: its token runs to `p` and any letters or digits after.
    fn bad_number(&self, start: usize, mut p: usize) -> Error {
        while p < self.b.len() && (self.b[p].is_ascii_alphanumeric() || self.b[p] == b'_') {
            p += 1;
        }
        json_error(
            self.s,
            p,
            false,
            format!("Token \"{}\" is invalid.", &self.s[start..p]),
        )
    }

    fn number(&mut self) -> Result<Jsonb> {
        let start = self.pos;
        let b = self.b;
        let mut p = self.pos;
        if b.get(p) == Some(&b'-') {
            p += 1;
        }
        let int_start = p;
        while p < b.len() && b[p].is_ascii_digit() {
            p += 1;
        }
        let int_len = p - int_start;
        let bad = int_len == 0 || (int_len > 1 && b[int_start] == b'0');
        if p < b.len() && b[p] == b'.' {
            p += 1;
            let f = p;
            while p < b.len() && b[p].is_ascii_digit() {
                p += 1;
            }
            if p == f {
                return Err(self.bad_number(start, p));
            }
        }
        if p < b.len() && (b[p] == b'e' || b[p] == b'E') {
            p += 1;
            if p < b.len() && (b[p] == b'+' || b[p] == b'-') {
                p += 1;
            }
            let e = p;
            while p < b.len() && b[p].is_ascii_digit() {
                p += 1;
            }
            if p == e {
                return Err(self.bad_number(start, p));
            }
        }
        if bad || (p < b.len() && (b[p].is_ascii_alphanumeric() || b[p] == b'_')) {
            return Err(self.bad_number(start, p));
        }
        self.pos = p;
        let text = &self.s[start..p];
        if !self.for_jsonb {
            return Ok(Jsonb::Number(Numeric::zero()));
        }
        // numeric_in: a lexically valid number can still overflow the numeric format.
        Ok(Jsonb::Number(Numeric::parse_pg(text)?))
    }

    fn string(&mut self) -> Result<String> {
        // Opening quote. An unterminated string is an invalid token (from the quote).
        let start = self.pos;
        let unterminated = |s: &str| {
            json_error(
                s,
                s.len(),
                false,
                format!("Token \"{}\" is invalid.", &s[start..]),
            )
        };
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.b.get(self.pos) else {
                return Err(unterminated(self.s));
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(&e) = self.b.get(self.pos) else {
                        return Err(unterminated(self.s));
                    };
                    self.pos += 1;
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
                            let cp = self.hex4()?;
                            // json only checks the escape's syntax.
                            if !self.for_jsonb {
                                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            } else if (0xD800..0xDC00).contains(&cp) {
                                // High surrogate: expect a low one.
                                if self.s[self.pos..].starts_with("\\u") {
                                    self.pos += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err(json_error(
                                            self.s,
                                            self.pos,
                                            false,
                                            "Unicode high surrogate must not follow a high surrogate.".into(),
                                        ));
                                    }
                                    let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                                } else {
                                    // Reported at the end of the character after it.
                                    let next = self.s[self.pos..]
                                        .chars()
                                        .next()
                                        .map_or(0, char::len_utf8);
                                    return Err(json_error(
                                        self.s,
                                        self.pos + next,
                                        false,
                                        "Unicode low surrogate must follow a high surrogate."
                                            .into(),
                                    ));
                                }
                            } else if (0xDC00..0xE000).contains(&cp) {
                                return Err(json_error(
                                    self.s,
                                    self.pos,
                                    false,
                                    "Unicode low surrogate must follow a high surrogate.".into(),
                                ));
                            } else if cp == 0 && self.for_jsonb {
                                return Err(Error::new(
                                    code::UNTRANSLATABLE_CHARACTER,
                                    "unsupported Unicode escape sequence",
                                )
                                .detail("\\u0000 cannot be converted to text.")
                                .context(json_context(self.s, self.pos, false)));
                            } else {
                                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            }
                        }
                        _ => {
                            let tok = &self.s[self.pos - 2..self.pos.min(self.s.len())];
                            return Err(json_error(
                                self.s,
                                self.pos,
                                false,
                                format!("Escape sequence \"{tok}\" is invalid."),
                            ));
                        }
                    }
                }
                c if c < 0x20 => {
                    let ch = c as char;
                    return Err(json_error(
                        self.s,
                        self.pos,
                        false,
                        format!("Character with value 0x{:02x} must be escaped.", ch as u32),
                    ));
                }
                _ => {
                    // Copy one UTF-8 character.
                    let ch = self.s[self.pos..].chars().next().unwrap();
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        let h = self
            .s
            .get(self.pos..self.pos + 4)
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
        match h {
            Some(h) => {
                self.pos += 4;
                Ok(u32::from_str_radix(h, 16).unwrap())
            }
            None => Err(json_error(
                self.s,
                (self.pos + 4).min(self.b.len()),
                false,
                "\"\\u\" must be followed by four hexadecimal digits.".into(),
            )),
        }
    }
}

pub fn parse_jsonb(s: &str) -> Result<Jsonb> {
    let mut p = Parser {
        s,
        b: s.as_bytes(),
        pos: 0,
        for_jsonb: true,
        depth: 0,
        dup: false,
    };
    let v = p.value()?;
    p.ws();
    if p.pos < p.b.len() {
        return Err(p.expected("end of input"));
    }
    Ok(v)
}

/// Validate `json` input (kept verbatim).
pub fn validate_json(s: &str) -> Result<()> {
    let mut p = Parser {
        s,
        b: s.as_bytes(),
        pos: 0,
        for_jsonb: false,
        depth: 0,
        dup: false,
    };
    p.value()?;
    p.ws();
    if p.pos < p.b.len() {
        return Err(p.expected("end of input"));
    }
    Ok(())
}

/// Parse json text, also telling whether some object in it repeats a key.
pub fn parse_json_dups(s: &str) -> Result<(Jsonb, bool)> {
    let mut p = Parser {
        s,
        b: s.as_bytes(),
        pos: 0,
        for_jsonb: false,
        depth: 0,
        dup: false,
    };
    let v = p.value()?;
    p.ws();
    if p.pos < p.b.len() {
        return Err(p.expected("end of input"));
    }
    Ok((v, p.dup))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let j =
            parse_jsonb(r#"{"b": [1, 2.50, true], "a": null, "aa": "x\"y", "a": 1e2}"#).unwrap();
        assert_eq!(
            j.to_string(),
            r#"{"a": 100, "b": [1, 2.50, true], "aa": "x\"y"}"#
        );
        assert!(parse_jsonb("{").is_err());
        assert!(parse_jsonb("[1,]").is_err());
        assert!(parse_jsonb("01").is_err());
        assert!(validate_json(r#"{"a":1}"#).is_ok());
        assert_eq!(
            parse_jsonb(r#""\u00e9\ud83d\ude00""#).unwrap().to_string(),
            "\"é😀\""
        );
    }

    #[test]
    fn containment() {
        let a = parse_jsonb(r#"{"a": 1, "b": [1, 2, 3], "c": {"d": 4}}"#).unwrap();
        assert!(a.contains(&parse_jsonb(r#"{"b": [3, 1]}"#).unwrap()));
        assert!(a.contains(&parse_jsonb(r#"{"c": {}}"#).unwrap()));
        assert!(!a.contains(&parse_jsonb(r#"{"a": 2}"#).unwrap()));
        assert!(
            parse_jsonb("[1, 2]")
                .unwrap()
                .contains(&parse_jsonb("1").unwrap())
        );
    }
}

/// jsonb_set_element with JB_PATH_CREATE | FILL_GAPS | CONSISTENT_POSITION (subscript
/// assignment, `j['a'][0] = v`): `root` with the element at `path` set to `value`, creating
/// what's missing. `root` None: the column was NULL (an empty array when `expect_array`,
/// else an empty object).
pub fn set_element(
    root: Option<&Jsonb>,
    path: &[String],
    value: Jsonb,
    expect_array: bool,
) -> crate::error::Result<Jsonb> {
    let empty;
    let root = match root {
        Some(r) => r,
        None => {
            empty = if expect_array {
                Jsonb::Array(Vec::new())
            } else {
                Jsonb::Object(Vec::new())
            };
            &empty
        }
    };
    set_level(root, path, 0, value)
}

fn path_index(path: &[String], level: usize) -> crate::error::Result<i64> {
    path[level].trim().parse::<i64>().map_err(|_| {
        crate::err!(
            INVALID_TEXT_REPRESENTATION,
            "path element at position {} is not an integer: \"{}\"",
            level + 1,
            path[level]
        )
    })
}

fn set_level(
    cur: &Jsonb,
    path: &[String],
    level: usize,
    value: Jsonb,
) -> crate::error::Result<Jsonb> {
    if level == path.len() {
        return Ok(value);
    }
    match cur {
        Jsonb::Object(pairs) => {
            let key = &path[level];
            let mut pairs = pairs.clone();
            match pairs.iter().position(|(k, _)| k == key) {
                Some(i) => {
                    let v = set_level(&pairs[i].1, path, level + 1, value)?;
                    pairs[i].1 = v;
                }
                None => {
                    let v = create_path(path, level + 1, value)?;
                    let at = pairs
                        .iter()
                        .position(|(k, _)| key_cmp(k, key) == Ordering::Greater)
                        .unwrap_or(pairs.len());
                    pairs.insert(at, (key.clone(), v));
                }
            }
            Ok(Jsonb::Object(pairs))
        }
        Jsonb::Array(items) => {
            let mut items = items.clone();
            let raw = path_index(path, level)?;
            let mut idx = raw;
            if idx < 0 {
                idx += items.len() as i64;
                if idx < 0 {
                    return Err(crate::err!(
                        INVALID_PARAMETER_VALUE,
                        "path element at position {} is out of range: {raw}",
                        level + 1
                    ));
                }
            }
            let idx = idx as usize;
            if idx < items.len() {
                let v = set_level(&items[idx], path, level + 1, value)?;
                items[idx] = v;
            } else {
                items.resize(idx, Jsonb::Null);
                items.push(create_path(path, level + 1, value)?);
            }
            Ok(Jsonb::Array(items))
        }
        _ => Err(crate::err!(
            INVALID_PARAMETER_VALUE,
            "cannot replace existing key"
        )
        .detail("The path assumes key is a composite object, but it is a scalar value.")),
    }
}

/// push_path: the rest of `path` built from nothing (an array for a numeric element, padded
/// with nulls; else an object).
fn create_path(path: &[String], level: usize, value: Jsonb) -> crate::error::Result<Jsonb> {
    if level == path.len() {
        return Ok(value);
    }
    let inner = create_path(path, level + 1, value)?;
    Ok(match path[level].trim().parse::<i64>() {
        Ok(i) if i >= 0 => {
            let mut items = vec![Jsonb::Null; i as usize];
            items.push(inner);
            Jsonb::Array(items)
        }
        _ => Jsonb::Object(vec![(path[level].clone(), inner)]),
    })
}
