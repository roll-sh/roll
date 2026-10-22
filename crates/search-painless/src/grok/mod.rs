//! Grok: named regular-expression macros (`%{IP:client} %{WORD:method}`) over a pattern bank,
//! as Elasticsearch's `grok` library implements them — for the `grok(...)` extractor of runtime
//! fields, and reusable by the ingest `grok` processor.
//!
//! A grok expression is expanded textually, Elasticsearch's way: the leftmost `%{NAME}`,
//! `%{NAME:field}` or `%{NAME:field:type}` (an inline `=definition` is accepted and ignored, as
//! there) is replaced by the bank's pattern for `NAME` — wrapped in a group named `NAME:field`
//! when it has a field, a non-capturing group otherwise — and the scan restarts at the
//! replacement, so references inside expand too. The result compiles in the regex engine's
//! Oniguruma mode ([`crate::regex::ONIG`]) because Elasticsearch matches with Joni, whose Ruby
//! syntax the bundled patterns are written for (group names with `:`, repeated names, `\h`,
//! POSIX brackets, line-anchored `^`/`$`).
//!
//! Matching searches anywhere in the text. Each distinct group name is one capture: its field
//! is the second `:`-separated part (or the whole name), its type the third (`int`, `long`,
//! `float`, `double`, `boolean`; anything else is a string). Every participating group of a
//! name contributes a value, and a field seen more than once collects a list, as Logstash does.
//!
//! The pattern banks are the `legacy` and `ecs-v1` sets Elasticsearch bundles, which come from
//! logstash-patterns-core (Apache License 2.0); they are embedded as data from `patterns/`.

use std::sync::{Arc, OnceLock};

use indexmap::IndexMap;

use crate::regex::{Regex, ONIG};

/// Why a grok expression could not be built (Elasticsearch's exception type and message).
#[derive(Clone, Debug, PartialEq)]
pub struct GrokError {
    /// `illegal_argument_exception`, or the regex engine's (`syntax_exception`,
    /// `value_exception`, `warning`).
    pub kind: &'static str,
    pub message: String,
}

impl GrokError {
    fn iae(message: impl Into<String>) -> GrokError {
        GrokError { kind: "illegal_argument_exception", message: message.into() }
    }
}

/// Named patterns, in definition order (a later definition of a name replaces the earlier).
#[derive(Clone, Debug, Default)]
pub struct PatternBank {
    patterns: IndexMap<String, String>,
}

const LEGACY: &[(&str, &str)] = &[
    ("aws", include_str!("patterns/legacy/aws")),
    ("bacula", include_str!("patterns/legacy/bacula")),
    ("bind", include_str!("patterns/legacy/bind")),
    ("bro", include_str!("patterns/legacy/bro")),
    ("exim", include_str!("patterns/legacy/exim")),
    ("firewalls", include_str!("patterns/legacy/firewalls")),
    ("grok-patterns", include_str!("patterns/legacy/grok-patterns")),
    ("haproxy", include_str!("patterns/legacy/haproxy")),
    ("httpd", include_str!("patterns/legacy/httpd")),
    ("java", include_str!("patterns/legacy/java")),
    ("junos", include_str!("patterns/legacy/junos")),
    ("linux-syslog", include_str!("patterns/legacy/linux-syslog")),
    ("maven", include_str!("patterns/legacy/maven")),
    ("mcollective-patterns", include_str!("patterns/legacy/mcollective-patterns")),
    ("mongodb", include_str!("patterns/legacy/mongodb")),
    ("nagios", include_str!("patterns/legacy/nagios")),
    ("postgresql", include_str!("patterns/legacy/postgresql")),
    ("rails", include_str!("patterns/legacy/rails")),
    ("redis", include_str!("patterns/legacy/redis")),
    ("ruby", include_str!("patterns/legacy/ruby")),
    ("squid", include_str!("patterns/legacy/squid")),
];

const ECS_V1: &[(&str, &str)] = &[
    ("aws", include_str!("patterns/ecs-v1/aws")),
    ("bacula", include_str!("patterns/ecs-v1/bacula")),
    ("bind", include_str!("patterns/ecs-v1/bind")),
    ("bro", include_str!("patterns/ecs-v1/bro")),
    ("exim", include_str!("patterns/ecs-v1/exim")),
    ("firewalls", include_str!("patterns/ecs-v1/firewalls")),
    ("grok-patterns", include_str!("patterns/ecs-v1/grok-patterns")),
    ("haproxy", include_str!("patterns/ecs-v1/haproxy")),
    ("httpd", include_str!("patterns/ecs-v1/httpd")),
    ("java", include_str!("patterns/ecs-v1/java")),
    ("junos", include_str!("patterns/ecs-v1/junos")),
    ("linux-syslog", include_str!("patterns/ecs-v1/linux-syslog")),
    ("maven", include_str!("patterns/ecs-v1/maven")),
    ("mcollective", include_str!("patterns/ecs-v1/mcollective")),
    ("mongodb", include_str!("patterns/ecs-v1/mongodb")),
    ("nagios", include_str!("patterns/ecs-v1/nagios")),
    ("postgresql", include_str!("patterns/ecs-v1/postgresql")),
    ("rails", include_str!("patterns/ecs-v1/rails")),
    ("redis", include_str!("patterns/ecs-v1/redis")),
    ("ruby", include_str!("patterns/ecs-v1/ruby")),
    ("squid", include_str!("patterns/ecs-v1/squid")),
    ("zeek", include_str!("patterns/ecs-v1/zeek")),
];

/// `GrokBuiltinPatterns.loadPatternsFromFile`: `NAME pattern` lines; blank lines and `#`
/// comments (after leading whitespace) are skipped, the name ends at the first whitespace run.
fn load(files: &[(&str, &str)]) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for (_, text) in files {
        for line in text.lines() {
            let t = line.trim_start();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let Some(i) = t.find(|c: char| c.is_whitespace()) else { continue };
            let rest = t[i..].trim_start();
            out.insert(t[..i].to_string(), rest.to_string());
        }
    }
    out
}

impl PatternBank {
    /// The `legacy` patterns (runtime fields; the grok processor without ECS compatibility).
    pub fn legacy() -> &'static PatternBank {
        static BANK: OnceLock<PatternBank> = OnceLock::new();
        BANK.get_or_init(|| PatternBank { patterns: load(LEGACY) })
    }

    /// The `ecs-v1` patterns (the grok processor with `ecs_compatibility: v1`).
    pub fn ecs_v1() -> &'static PatternBank {
        static BANK: OnceLock<PatternBank> = OnceLock::new();
        BANK.get_or_init(|| PatternBank { patterns: load(ECS_V1) })
    }

    /// A bank of the given patterns, rejecting references to missing patterns and cycles with
    /// Elasticsearch's messages.
    pub fn new(patterns: IndexMap<String, String>) -> Result<PatternBank, GrokError> {
        forbid_circular_references(&patterns)?;
        Ok(PatternBank { patterns })
    }

    /// This bank plus `extra` (`pattern_definitions` of the grok processor), validated as a
    /// whole.
    pub fn extend_with(&self, extra: &[(String, String)]) -> Result<PatternBank, GrokError> {
        if extra.is_empty() {
            return Ok(self.clone());
        }
        let mut all = self.patterns.clone();
        for (k, v) in extra {
            all.insert(k.clone(), v.clone());
        }
        PatternBank::new(all)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.patterns.get(name).map(String::as_str)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.patterns.keys().map(String::as_str)
    }
}

/// `PatternBank.getPatternNamesForPattern`: the distinct names a pattern references.
fn references(bank: &IndexMap<String, String>, name: &str) -> Result<Vec<String>, GrokError> {
    let pattern = &bank[name];
    let mut out: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(off) = pattern[from..].find("%{") {
        let i = from + off;
        let begin = i + 2;
        let bracket = pattern[begin..].find('}').map(|j| begin + j);
        let colon = pattern[begin..].find(':').map(|j| begin + j);
        let end = match (bracket, colon) {
            (Some(b), None) => b,
            (None, Some(c)) => c,
            (Some(b), Some(c)) => b.min(c),
            (None, None) => return Err(GrokError::iae(format!("pattern [{pattern}] has an invalid syntax"))),
        };
        let other = &pattern[begin..end];
        if !out.iter().any(|o| o == other) {
            if !bank.contains_key(other) {
                return Err(GrokError::iae(format!("pattern [{name}] is referencing a non-existent pattern [{other}]")));
            }
            out.push(other.to_string());
        }
        from = i + 1;
    }
    Ok(out)
}

/// `PatternBank.forbidCircularReferences`: a depth-first walk from every pattern; reaching the
/// start again reports the path.
fn forbid_circular_references(bank: &IndexMap<String, String>) -> Result<(), GrokError> {
    use std::collections::HashSet;
    let mut all_visited: HashSet<String> = HashSet::new();
    let mut revisited: HashSet<String> = HashSet::new();
    for start in bank.keys() {
        if !revisited.contains(start) && all_visited.contains(start) {
            continue;
        }
        let mut visited: IndexMap<String, ()> = IndexMap::new();
        // each level: the names still to visit there (the first is current)
        let mut stack: Vec<Vec<Option<String>>> = vec![vec![Some(start.clone())]];
        let mut unwinding = false;
        while let Some(level) = stack.last() {
            let first = level.iter().position(Option::is_some).expect("a level always has a node");
            let node = level[first].clone().unwrap();
            let last_index = level.len() - 1;
            let mut end_of_path = false;
            if unwinding {
                end_of_path = true;
            } else if node == *start && stack.len() > 1 {
                let path: Vec<String> = stack.iter().map(|l| l.iter().flatten().next().unwrap().clone()).collect();
                return Err(GrokError::iae(format!("circular reference detected: {}", path.join("->"))));
            } else if visited.contains_key(&node) {
                revisited.insert(node.clone());
                end_of_path = true;
            } else {
                visited.insert(node.clone(), ());
                let next = references(bank, &node)?;
                if next.is_empty() {
                    end_of_path = true;
                } else {
                    stack.push(next.into_iter().map(Some).collect());
                }
            }
            if end_of_path {
                let level = stack.last_mut().unwrap();
                if first == last_index {
                    stack.pop();
                    unwinding = true;
                } else {
                    level[first] = None;
                    unwinding = false;
                }
            } else {
                unwinding = false;
            }
        }
        all_visited.extend(visited.into_keys());
    }
    Ok(())
}

/// The type a capture converts its text to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureType {
    String,
    Int,
    Long,
    Float,
    Double,
    Boolean,
}

impl CaptureType {
    fn of(name: &str) -> CaptureType {
        match name {
            "int" => CaptureType::Int,
            "long" => CaptureType::Long,
            "float" => CaptureType::Float,
            "double" => CaptureType::Double,
            "boolean" => CaptureType::Boolean,
            _ => CaptureType::String,
        }
    }
}

/// One named capture: a field fed by every group with that name.
#[derive(Clone, Debug)]
pub struct Capture {
    pub name: String,
    pub ty: CaptureType,
    groups: Vec<usize>,
}

/// A captured value.
#[derive(Clone, Debug, PartialEq)]
pub enum GrokValue {
    Str(String),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Bool(bool),
    /// A field captured more than once.
    List(Vec<GrokValue>),
}

/// A capture whose text does not parse as its type (`Integer.parseInt` and friends failing).
#[derive(Clone, Debug, PartialEq)]
pub struct ConvertError {
    pub ty: CaptureType,
    pub text: String,
}

/// A compiled grok expression. Immutable; share it across threads.
#[derive(Debug)]
pub struct Grok {
    regex: Arc<Regex>,
    captures: Vec<Capture>,
    expression: String,
}

/// The parts of one `%{...}` reference.
struct Reference<'a> {
    start: usize,
    end: usize,
    /// `NAME` or `NAME:subname`.
    name: &'a str,
    pattern: &'a str,
    subname: Option<&'a str>,
}

fn is_pattern_char(c: char) -> bool {
    // `[A-z0-9]`: the ASCII range A..z includes `[ \ ] ^ _` and the backtick
    c.is_ascii_digit() || ('A'..='z').contains(&c)
}

fn is_subname_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '@' | '[' | ']' | '_' | ':' | '.' | '-')
}

/// The leftmost reference in `s` (Elasticsearch's `GROK_PATTERN` regex, searched).
fn find_reference(s: &str) -> Option<Reference<'_>> {
    let mut from = 0;
    while let Some(off) = s[from..].find("%{") {
        let start = from + off;
        if let Some(r) = reference_at(s, start) {
            return Some(r);
        }
        from = start + 1;
    }
    None
}

fn reference_at(s: &str, start: usize) -> Option<Reference<'_>> {
    let body = start + 2;
    let mut i = body;
    let chars = |from: usize| s[from..].char_indices().map(move |(j, c)| (from + j, c));
    let mut name_end = body;
    for (j, c) in chars(body) {
        if is_pattern_char(c) {
            name_end = j + c.len_utf8();
        } else {
            break;
        }
    }
    if name_end == body {
        return None;
    }
    let pattern = &s[body..name_end];
    i = name_end.max(i);
    let mut subname = None;
    if s[i..].starts_with(':') {
        let sb = i + 1;
        let mut se = sb;
        for (j, c) in chars(sb) {
            if is_subname_char(c) {
                se = j + c.len_utf8();
            } else {
                break;
            }
        }
        if se > sb {
            subname = Some(&s[sb..se]);
            i = se;
        }
    }
    let name_part_end = i;
    if s[i..].starts_with('=') {
        // `(?:[^{}]+|\.+)+`: anything but braces, then the closing brace must follow
        let db = i + 1;
        let de = s[db..].find(['{', '}']).map(|j| db + j).unwrap_or(s.len());
        if de > db {
            i = de;
        }
    }
    if !s[i..].starts_with('}') {
        return None;
    }
    Some(Reference { start, end: i + 1, name: &s[body..name_part_end], pattern, subname })
}

impl Grok {
    /// Compiles `expression` against `bank`.
    pub fn new(bank: &PatternBank, expression: &str) -> Result<Grok, GrokError> {
        let regex_src = to_regex(bank, expression)?;
        let regex = Regex::compile(&regex_src, ONIG).map_err(|e| GrokError { kind: e.kind, message: e.description })?;
        // one capture per distinct group name, in definition order
        let mut captures: Vec<(String, Capture)> = Vec::new();
        for (group, idx) in regex.group_names() {
            match captures.iter_mut().find(|(g, _)| g == group) {
                Some((_, c)) => c.groups.push(*idx),
                None => {
                    // `String.split(":")`: trailing empty parts are dropped
                    let mut parts: Vec<&str> = group.split(':').collect();
                    while parts.len() > 1 && parts.last() == Some(&"") {
                        parts.pop();
                    }
                    let name = if parts.len() >= 2 { parts[1] } else { parts[0] };
                    let ty = if parts.len() == 3 { CaptureType::of(parts[2]) } else { CaptureType::String };
                    captures.push((group.clone(), Capture { name: name.to_string(), ty, groups: vec![*idx] }));
                }
            }
        }
        Ok(Grok { regex: Arc::new(regex), captures: captures.into_iter().map(|(_, c)| c).collect(), expression: expression.to_string() })
    }

    /// Joni's warnings for the expanded expression (redundant nested repeats, unknown
    /// escapes). Runtime fields refuse a grok expression that has any; the grok processor
    /// does not.
    pub fn warnings(&self) -> &[String] {
        self.regex.warnings()
    }

    pub fn expression(&self) -> &str {
        &self.expression
    }

    pub fn captures_config(&self) -> &[Capture] {
        &self.captures
    }

    /// Whether the expression matches anywhere in `text`.
    pub fn matches(&self, text: &str) -> bool {
        let mut m = crate::regex::MatcherState::new(self.regex.clone(), text, 0);
        m.find().unwrap_or(false)
    }

    /// The named captures of the first match, in capture order (`None` when nothing matches).
    pub fn captures(&self, text: &str) -> Result<Option<Vec<(String, GrokValue)>>, ConvertError> {
        let mut m = crate::regex::MatcherState::new(self.regex.clone(), text, 0);
        if !m.find().unwrap_or(false) {
            return Ok(None);
        }
        let mut out: Vec<(String, GrokValue)> = Vec::new();
        for c in &self.captures {
            for &g in &c.groups {
                let Ok(Some(s)) = m.group(g as i32) else { continue };
                let v = convert(c.ty, s)?;
                match out.iter_mut().find(|(k, _)| *k == c.name) {
                    Some((_, GrokValue::List(l))) => l.push(v),
                    Some((_, old)) => {
                        let first = std::mem::replace(old, GrokValue::Bool(false));
                        *old = GrokValue::List(vec![first, v]);
                    }
                    None => out.push((c.name.clone(), v)),
                }
            }
        }
        Ok(Some(out))
    }
}

fn convert(ty: CaptureType, s: String) -> Result<GrokValue, ConvertError> {
    let fail = |s: String| ConvertError { ty, text: s };
    Ok(match ty {
        CaptureType::String => GrokValue::Str(s),
        CaptureType::Int => match crate::java::parse_int(&s, 10) {
            Some(v) => GrokValue::Int(v),
            None => return Err(fail(s)),
        },
        CaptureType::Long => match crate::java::parse_long(&s, 10) {
            Some(v) => GrokValue::Long(v),
            None => return Err(fail(s)),
        },
        CaptureType::Float => match crate::java::parse_float(&s) {
            Some(v) => GrokValue::Float(v),
            None => return Err(fail(s)),
        },
        CaptureType::Double => match crate::java::parse_double(&s) {
            Some(v) => GrokValue::Double(v),
            None => return Err(fail(s)),
        },
        CaptureType::Boolean => GrokValue::Bool(s.eq_ignore_ascii_case("true")),
    })
}

/// `Grok.toRegex`: expands references left to right, rescanning each replacement.
fn to_regex(bank: &PatternBank, expression: &str) -> Result<String, GrokError> {
    let mut res = String::new();
    let mut rest = expression.to_string();
    for _ in 0..100_000 {
        let Some(r) = find_reference(&rest) else {
            res.push_str(&rest);
            return Ok(res);
        };
        let Some(pattern) = bank.get(r.pattern) else {
            return Err(GrokError::iae(format!("Unable to find pattern [{}] in Grok's pattern dictionary", r.pattern)));
        };
        if pattern.contains(&format!("%{{{}}}", r.pattern)) || pattern.contains(&format!("%{{{}:", r.pattern)) {
            return Err(GrokError::iae(format!("circular reference in pattern back [{}]", r.pattern)));
        }
        let part = if r.subname.is_some() { format!("(?<{}>{})", r.name, pattern) } else { format!("(?:{pattern})") };
        res.push_str(&rest[..r.start]);
        rest = format!("{part}{}", &rest[r.end..]);
    }
    Err(GrokError::iae("Can not convert grok patterns to regular expression"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(expr: &str, text: &str) -> Option<Vec<(String, GrokValue)>> {
        Grok::new(PatternBank::legacy(), expr).unwrap().captures(text).unwrap()
    }

    fn s(v: &str) -> GrokValue {
        GrokValue::Str(v.to_string())
    }

    #[test]
    fn banks_load() {
        assert!(PatternBank::legacy().get("COMBINEDAPACHELOG").is_some());
        assert!(PatternBank::ecs_v1().get("ECS_COMBINEDAPACHELOG").is_none());
        assert_eq!(PatternBank::legacy().get("WORD"), Some(r"\b\w+\b"));
        // every bundled pattern expands and compiles — except the ECS ones whose bracket escapes
        // Elasticsearch's copy mangled (`\%{HTTPDATE:timestamp}\ `): they fail there too
        let mut broken = Vec::new();
        for bank in [PatternBank::legacy(), PatternBank::ecs_v1()] {
            for name in bank.names() {
                if let Err(e) = Grok::new(bank, &format!("%{{{name}}}")) {
                    broken.push((name.to_string(), e.message));
                }
            }
        }
        let want = [
            ("S3_ACCESS_LOG", "unmatched close parenthesis"),
            ("EXIM_REMOTE_HOST", "unmatched close parenthesis"),
            ("EXIM_NAMED_FIELDS", "unmatched close parenthesis"),
            ("EXIM_MESSAGE_ARRIVAL", "unmatched close parenthesis"),
            ("EXIM", "unmatched close parenthesis"),
            ("CISCOFW106023", "premature end of char-class"),
            ("CISCOFW106100_2_3", "premature end of char-class"),
            ("CISCOFW106100", "premature end of char-class"),
        ];
        assert_eq!(broken, want.map(|(a, b)| (a.to_string(), b.to_string())));
    }

    #[test]
    fn captures_fields_types_and_repeats() {
        assert_eq!(caps("%{IP:client} %{WORD:method}", "55.3.244.1 GET"), Some(vec![("client".into(), s("55.3.244.1")), ("method".into(), s("GET"))]));
        assert_eq!(caps("%{NUMBER:n:int} %{NUMBER:f:float}", "5 1.5"), Some(vec![("n".into(), GrokValue::Int(5)), ("f".into(), GrokValue::Float(1.5))]));
        assert_eq!(caps("%{WORD:a} %{WORD:a}", "x y"), Some(vec![("a".into(), GrokValue::List(vec![s("x"), s("y")]))]));
        assert_eq!(caps("%{WORD:a}(?: %{WORD:a})?", "x"), Some(vec![("a".into(), s("x"))]));
        assert_eq!(caps("%{NUMBER}", "42"), Some(vec![]));
        assert_eq!(caps("%{NUMBER:x}", "nope"), None);
        assert_eq!(caps("(?<x:int>\\d+)", "12"), Some(vec![("int".into(), s("12"))]));
        // Joni: `\w` is ASCII but `\b` knows Unicode letters
        assert_eq!(caps("%{WORD:w}", "héllo wörld"), None);
        assert_eq!(caps("%{WORD:w}", "éa b"), Some(vec![("w".into(), s("b"))]));
        assert!(Grok::new(PatternBank::legacy(), "%{NUMBER:n:int}").unwrap().captures("1.5").is_err());
    }

    #[test]
    fn errors() {
        let e = |p: &str| Grok::new(PatternBank::legacy(), p).unwrap_err();
        assert_eq!(e("%{FOO:x}").message, "Unable to find pattern [FOO] in Grok's pattern dictionary");
        assert_eq!(e("(%{WORD:x}"), GrokError { kind: "syntax_exception", message: "end pattern with unmatched parenthesis".into() });
        assert_eq!(e("a)").message, "unmatched close parenthesis");
        let w = Grok::new(PatternBank::legacy(), "a**").unwrap();
        assert_eq!(w.warnings(), ["regular expression has redundant nested repeat operator * /a**/"]);
        let w = Grok::new(PatternBank::legacy(), "(?<x>a?*)").unwrap();
        assert_eq!(w.warnings(), ["nested repeat operator '?' and '*' was replaced with '*' in regular expression /(?<x>a?*)/"]);
        assert!(Grok::new(PatternBank::legacy(), "(?<x>(a*)*)").unwrap().warnings().is_empty());
        // an unterminated reference is plain text
        assert_eq!(Grok::new(PatternBank::legacy(), "%{WORD:x").unwrap().captures("a").unwrap(), None);
        let mut m = IndexMap::new();
        m.insert("A".to_string(), "%{B}".to_string());
        m.insert("B".to_string(), "%{A}".to_string());
        assert_eq!(PatternBank::new(m).unwrap_err().message, "circular reference detected: A->B->A");
        let mut m = IndexMap::new();
        m.insert("A".to_string(), "%{C}".to_string());
        assert_eq!(PatternBank::new(m).unwrap_err().message, "pattern [A] is referencing a non-existent pattern [C]");
    }
}
