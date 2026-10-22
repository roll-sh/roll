//! `filter_path`: Elasticsearch's response filtering.
//!
//! Filters are dotted paths of field names with `*` globs and `**` (any depth); a leading `-`
//! makes a filter an exclusion, and `\.` escapes a dot inside a name. Semantics follow
//! Elasticsearch's `FilterPath` + `FilterPathBasedFilter` over Jackson's filtering generator:
//!
//! - Arrays are transparent: a filter applies to each element.
//! - Inclusive filtering keeps a field when a filter ends on it (with everything below), drops
//!   scalars reached by a partial match, and drops objects and arrays left empty. If nothing
//!   matches, the response is `{}`.
//! - Exclusive filtering removes the fields a filter ends on; containers emptied by it are
//!   dropped, containers that were already empty are kept.
//! - Inclusions apply first, then exclusions.
//!
//! The filter is a compiled tree applied to the response value before it is serialized.

use crate::json::{Map, Value};
use std::collections::HashMap;

#[derive(Debug, Default)]
struct Node {
    /// The pattern of this node (`""` for the root).
    pattern: String,
    is_final: bool,
    is_double_wildcard: bool,
    terms: HashMap<String, Node>,
    wildcards: Vec<Node>,
}

#[derive(Debug, Default)]
struct BuildNode {
    is_final: bool,
    children: Vec<(String, BuildNode)>,
}

impl BuildNode {
    fn child(&mut self, name: &str) -> &mut BuildNode {
        if let Some(i) = self.children.iter().position(|(n, _)| n == name) {
            return &mut self.children[i].1;
        }
        self.children.push((name.to_string(), BuildNode::default()));
        &mut self.children.last_mut().unwrap().1
    }

    fn insert(&mut self, filter: &str, depth: usize) -> Result<(), String> {
        if depth > 500 {
            let cut: String = filter.chars().take(100).collect();
            return Err(format!("Filter exceeds maximum depth at [{cut}]"));
        }
        let bytes = filter.as_bytes();
        let mut split = None;
        let mut escapes = false;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'.' => {
                    split = Some(i);
                    break;
                }
                b'\\' if bytes.get(i + 1) == Some(&b'.') => {
                    i += 1;
                    escapes = true;
                }
                _ => {}
            }
            i += 1;
        }
        let unescape = |s: &str| if escapes { s.replace("\\.", ".") } else { s.to_string() };
        match split {
            Some(pos) if pos > 0 => {
                let field = unescape(&filter[..pos]);
                let child = self.child(&field);
                if !child.is_final {
                    child.insert(&filter[pos + 1..], depth + 1)?;
                }
            }
            _ => {
                let field = unescape(filter);
                let child = self.child(&field);
                *child = BuildNode { is_final: true, children: Vec::new() };
            }
        }
        Ok(())
    }

    fn build(self, pattern: String) -> Node {
        let mut node =
            Node { is_double_wildcard: pattern == "**", pattern, is_final: self.is_final, terms: HashMap::new(), wildcards: Vec::new() };
        for (name, child) in self.children {
            let c = child.build(name.clone());
            if name.contains('*') {
                node.wildcards.push(c);
            } else {
                node.terms.insert(name, c);
            }
        }
        node
    }
}

impl Node {
    /// `FilterPath.matches`: `true` if a final node matches `name`; otherwise collects the
    /// nodes to continue with below `name`.
    fn matches<'a>(&'a self, name: &str, next: &mut Vec<&'a Node>) -> bool {
        if let Some(t) = self.terms.get(name) {
            if t.is_final {
                return true;
            }
            next.push(t);
        }
        for w in &self.wildcards {
            if glob_match(&w.pattern, name) {
                if w.is_final {
                    return true;
                }
                next.push(w);
            }
        }
        if self.is_double_wildcard {
            next.push(self);
        }
        false
    }
}

enum Eval<'a> {
    Match,
    NoMatch,
    Partial(Vec<&'a Node>),
}

fn evaluate<'a>(name: &str, filters: &[&'a Node]) -> Eval<'a> {
    let mut next = Vec::new();
    for f in filters {
        if f.matches(name, &mut next) {
            return Eval::Match;
        }
    }
    if next.is_empty() { Eval::NoMatch } else { Eval::Partial(next) }
}

/// Elasticsearch's `Glob.globMatch` (`*` matches any run of characters).
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let p = pattern.as_bytes();
    let s = s.as_bytes();
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = pi;
            pi += 1;
            mark = si;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// A compiled `filter_path`.
#[derive(Debug, Default)]
pub struct FilterPath {
    include: Option<Node>,
    exclude: Option<Node>,
}

impl FilterPath {
    /// Parses the comma-separated `filter_path` parameter. `None` if it holds no filter.
    pub fn parse(param: &str) -> Result<Option<FilterPath>, String> {
        let mut inc = BuildNode::default();
        let mut exc = BuildNode::default();
        let (mut has_inc, mut has_exc) = (false, false);
        for f in param.split(',') {
            let f = f.trim();
            if f.is_empty() {
                continue;
            }
            if let Some(e) = f.strip_prefix('-') {
                let e = e.trim();
                if !e.is_empty() {
                    exc.insert(e, 0)?;
                    has_exc = true;
                }
            } else {
                inc.insert(f, 0)?;
                has_inc = true;
            }
        }
        if !has_inc && !has_exc {
            return Ok(None);
        }
        Ok(Some(FilterPath {
            include: has_inc.then(|| inc.build(String::new())),
            exclude: has_exc.then(|| exc.build(String::new())),
        }))
    }

    /// Filters a response body.
    pub fn apply(&self, v: Value) -> Value {
        let mut v = v;
        if let Some(inc) = &self.include {
            v = match v {
                Value::Obj(m) => Value::Obj(include_obj(m, &[inc]).unwrap_or_default()),
                Value::Arr(a) => Value::Arr(include_arr(a, &[inc]).unwrap_or_default()),
                other => other,
            };
        }
        if let Some(exc) = &self.exclude {
            v = match v {
                Value::Obj(m) => Value::Obj(exclude_obj(m, &[exc]).unwrap_or_default()),
                Value::Arr(a) => Value::Arr(exclude_arr(a, &[exc]).unwrap_or_default()),
                other => other,
            };
        }
        v
    }
}

fn include_obj(m: Map, filters: &[&Node]) -> Option<Map> {
    let mut out = Map::new();
    for (k, v) in m {
        match evaluate(&k, filters) {
            Eval::Match => {
                out.insert(k, v);
            }
            Eval::NoMatch => {}
            Eval::Partial(next) => {
                if let Some(v) = include_value(v, &next) {
                    out.insert(k, v);
                }
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

fn include_arr(a: Vec<Value>, filters: &[&Node]) -> Option<Vec<Value>> {
    let out: Vec<Value> = a.into_iter().filter_map(|v| include_value(v, filters)).collect();
    (!out.is_empty()).then_some(out)
}

fn include_value(v: Value, filters: &[&Node]) -> Option<Value> {
    match v {
        Value::Obj(m) => include_obj(m, filters).map(Value::Obj),
        Value::Arr(a) => include_arr(a, filters).map(Value::Arr),
        _ => None,
    }
}

fn exclude_obj(m: Map, filters: &[&Node]) -> Option<Map> {
    let was_empty = m.is_empty();
    let mut out = Map::new();
    let mut filtered = false;
    for (k, v) in m {
        match evaluate(&k, filters) {
            Eval::Match => filtered = true,
            Eval::NoMatch => {
                out.insert(k, v);
            }
            Eval::Partial(next) => match exclude_value(v, &next) {
                Some(v) => {
                    out.insert(k, v);
                }
                None => filtered = true,
            },
        }
    }
    if out.is_empty() && !was_empty && filtered {
        return None;
    }
    Some(out)
}

fn exclude_arr(a: Vec<Value>, filters: &[&Node]) -> Option<Vec<Value>> {
    let was_empty = a.is_empty();
    let mut filtered = false;
    let mut out = Vec::with_capacity(a.len());
    for v in a {
        match exclude_value(v, filters) {
            Some(v) => out.push(v),
            None => filtered = true,
        }
    }
    if out.is_empty() && !was_empty && filtered {
        return None;
    }
    Some(out)
}

fn exclude_value(v: Value, filters: &[&Node]) -> Option<Value> {
    match v {
        Value::Obj(m) => exclude_obj(m, filters).map(Value::Obj),
        Value::Arr(a) => exclude_arr(a, filters).map(Value::Arr),
        scalar => Some(scalar),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn run(filter: &str, doc: &str) -> String {
        let v = parse(doc.as_bytes()).unwrap().unwrap();
        FilterPath::parse(filter).unwrap().unwrap().apply(v).to_json()
    }

    const DOC: &str = r#"{"took":3,"hits":{"total":{"value":2},"hits":[{"_id":"1","_source":{"a":1,"b.c":2}},{"_id":"2","_score":1.0}]},"empty":{},"list":[1,2]}"#;

    #[test]
    fn includes() {
        assert_eq!(run("took", DOC), r#"{"took":3}"#);
        assert_eq!(run("hits.hits._id", DOC), r#"{"hits":{"hits":[{"_id":"1"},{"_id":"2"}]}}"#);
        assert_eq!(run("hits.total.value,took", DOC), r#"{"took":3,"hits":{"total":{"value":2}}}"#);
        assert_eq!(run("**._id", DOC), r#"{"hits":{"hits":[{"_id":"1"},{"_id":"2"}]}}"#);
        assert_eq!(run("hits.*.value", DOC), r#"{"hits":{"total":{"value":2}}}"#);
        assert_eq!(run("t*", DOC), r#"{"took":3}"#);
        assert_eq!(run("nope", DOC), "{}");
        assert_eq!(run("empty", DOC), r#"{"empty":{}}"#);
        assert_eq!(run("empty.x", DOC), "{}");
        assert_eq!(run("list.x", DOC), "{}");
        assert_eq!(run(r"hits.hits._source.b\.c", DOC), r#"{"hits":{"hits":[{"_source":{"b.c":2}}]}}"#);
    }

    #[test]
    fn excludes() {
        assert_eq!(run("-hits", DOC), r#"{"took":3,"empty":{},"list":[1,2]}"#);
        assert_eq!(
            run("-hits.hits._source,-took", DOC),
            r#"{"hits":{"total":{"value":2},"hits":[{"_id":"1"},{"_id":"2","_score":1.0}]},"empty":{},"list":[1,2]}"#
        );
        assert!(!run("-hits.total.value", DOC).contains(r#""total""#));
        assert!(run("-empty.x", DOC).contains(r#""empty":{}"#));
    }

    #[test]
    fn mixed() {
        assert_eq!(run("hits.hits,-hits.hits._source", DOC), r#"{"hits":{"hits":[{"_id":"1"},{"_id":"2","_score":1.0}]}}"#);
    }

    #[test]
    fn globs() {
        assert!(glob_match("*", "abc"));
        assert!(glob_match("a*c", "abbbc"));
        assert!(glob_match("*b*", "abc"));
        assert!(!glob_match("a*d", "abc"));
        assert!(glob_match("**", "x"));
    }
}
