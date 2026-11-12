//! Binding `#name` and `:value` references, and the checks ScyllaDB makes while doing so.
//!
//! References are resolved left to right in source order. A missing name or value is reported
//! as `ExpressionAttributeNames missing entry '#n' required by expression` (with an extra comma,
//! `missing, entry`, when the whole map is absent); a value is validated as a top-level
//! attribute value with the `ExpressionAttributeValues:` prefix. After every expression of a
//! request has been resolved, an entry nobody used is `... has spurious '#n', not used in <Op>`
//! (names first, then values, the first unused entry in request order).

use std::collections::HashSet;

use super::ast::{Name, Path, PathOp};
use crate::error::{Error, Result};
use crate::json::Json;
use crate::value;

/// Maximum length of an attribute name.
pub const MAX_NAME_BYTES: usize = 65535;

/// Maximum number of components of a document path (the attribute and its nested steps).
pub const MAX_PATH_DEPTH: usize = 32;

/// Maximum size of any expression.
pub const MAX_EXPRESSION_BYTES: usize = 4096;

/// A path with names resolved.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RPath {
    pub root: String,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    Field(String),
    Index(u32),
}

impl RPath {
    pub fn top(name: impl Into<String>) -> RPath {
        RPath {
            root: name.into(),
            steps: Vec::new(),
        }
    }

    pub fn is_top(&self) -> bool {
        self.steps.is_empty()
    }
}

impl std::fmt::Display for RPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.root)?;
        for s in &self.steps {
            match s {
                Step::Field(n) => write!(f, ".{n}")?,
                Step::Index(i) => write!(f, "[{i}]")?,
            }
        }
        Ok(())
    }
}

/// Checks an attribute name as ScyllaDB does wherever names are given (`Empty attribute name`,
/// `Attribute name is too large, must be less than 65536 bytes`).
pub fn check_attr_name(name: &str) -> std::result::Result<(), &'static str> {
    if name.is_empty() {
        Err("Empty attribute name")
    } else if name.len() > MAX_NAME_BYTES {
        Err("Attribute name is too large, must be less than 65536 bytes")
    } else {
        Ok(())
    }
}

/// ExpressionAttributeNames / ExpressionAttributeValues of a request, with use tracking.
pub struct Bindings<'a> {
    pub names: Option<&'a Json>,
    pub values: Option<&'a Json>,
    used_names: HashSet<String>,
    used_values: HashSet<String>,
}

impl<'a> Bindings<'a> {
    pub fn new(body: &'a Json) -> Bindings<'a> {
        Bindings {
            names: body.get("ExpressionAttributeNames"),
            values: body.get("ExpressionAttributeValues"),
            used_names: HashSet::new(),
            used_values: HashSet::new(),
        }
    }

    pub fn name(&mut self, r: &str) -> Result<String> {
        let found = self.names.and_then(|m| m.get(r)).and_then(Json::as_str);
        let Some(n) = found else {
            let comma = if self.names.is_none() { "," } else { "" };
            return Err(Error::validation(format!(
                "ExpressionAttributeNames missing{comma} entry '{r}' required by expression"
            )));
        };
        if let Err(e) = check_attr_name(n) {
            return Err(Error::validation(format!(
                "ExpressionAttributeNames contains invalid value: {e}"
            )));
        }
        self.used_names.insert(r.to_string());
        Ok(n.to_string())
    }

    pub fn value(&mut self, r: &str) -> Result<Json> {
        let Some(v) = self.values.and_then(|m| m.get(r)) else {
            let comma = if self.values.is_none() { "," } else { "" };
            return Err(Error::validation(format!(
                "ExpressionAttributeValues missing{comma} entry '{r}' required by expression"
            )));
        };
        value::validate_shape(v, "ExpressionAttributeValues")?;
        self.used_values.insert(r.to_string());
        Ok(v.clone())
    }

    fn resolve_name(&mut self, n: &Name) -> Result<String> {
        match n {
            Name::Plain(s) => Ok(s.clone()),
            Name::Ref(r) => self.name(r),
        }
    }

    pub fn path(&mut self, p: &Path) -> Result<RPath> {
        let root = self.resolve_name(&p.root)?;
        let mut steps = Vec::with_capacity(p.ops.len());
        for op in &p.ops {
            steps.push(match op {
                PathOp::Field(n) => Step::Field(self.resolve_name(n)?),
                PathOp::Index(i) => Step::Index(*i),
            });
        }
        Ok(RPath { root, steps })
    }

    /// The `spurious` check, run once all of a request's expressions are resolved.
    pub fn check_unused(&self, op: &str) -> Result<()> {
        if let Some(Json::Object(m)) = self.names
            && let Some((k, _)) = m.iter().find(|(k, _)| !self.used_names.contains(k))
        {
            return Err(Error::validation(format!(
                "ExpressionAttributeNames has spurious '{k}', not used in {op}"
            )));
        }
        if let Some(Json::Object(m)) = self.values
            && let Some((k, _)) = m.iter().find(|(k, _)| !self.used_values.contains(k))
        {
            return Err(Error::validation(format!(
                "ExpressionAttributeValues has spurious '{k}', not used in {op}"
            )));
        }
        Ok(())
    }

    /// Marks every name and value as used (operations that don't check them).
    pub fn mark_all_names_used(&mut self) {
        if let Some(Json::Object(m)) = self.names {
            self.used_names.extend(m.iter().map(|(k, _)| k.clone()));
        }
    }

    pub fn mark_all_values_used(&mut self) {
        if let Some(Json::Object(m)) = self.values {
            self.used_values.extend(m.iter().map(|(k, _)| k.clone()));
        }
    }
}

/// Rejects an expression over the size limit (checked before parsing).
pub fn check_size(kind: &str, src: &str) -> Result<()> {
    if src.len() > MAX_EXPRESSION_BYTES {
        return Err(Error::validation(format!(
            "{kind} expression size {} exceeds allowed maximum {MAX_EXPRESSION_BYTES}.",
            src.len()
        )));
    }
    Ok(())
}

/// A set of document paths that must not overlap or conflict (ProjectionExpression, the
/// targets of an UpdateExpression).
#[derive(Default)]
pub struct PathSet {
    roots: Vec<(String, Node)>,
}

#[derive(Default)]
struct Node {
    leaf: bool,
    fields: Vec<(String, Node)>,
    indexes: Vec<(u32, Node)>,
}

impl PathSet {
    /// Adds a path; `Err(Some(true))`-style results are reported by the caller with the kind
    /// of expression in the message.
    pub fn insert(&mut self, p: &RPath) -> std::result::Result<(), (bool, String)> {
        let overlap = |s: String| Err((true, s));
        let conflict = |s: String| Err((false, s));
        let pos = self.roots.iter().position(|(n, _)| *n == p.root);
        let root = match pos {
            Some(i) => &mut self.roots[i].1,
            None => {
                self.roots.push((p.root.clone(), Node::default()));
                &mut self.roots.last_mut().expect("just pushed").1
            }
        };
        if p.steps.is_empty() {
            if pos.is_some() {
                return overlap(p.root.clone());
            }
            root.leaf = true;
            return Ok(());
        }
        if root.leaf {
            return overlap(p.root.clone());
        }
        let mut node = root;
        for (i, step) in p.steps.iter().enumerate() {
            let last = i + 1 == p.steps.len();
            node = match step {
                Step::Field(f) => {
                    if !node.indexes.is_empty() {
                        return conflict(p.to_string());
                    }
                    match node.fields.iter().position(|(n, _)| n == f) {
                        Some(j) => {
                            let child = &mut node.fields[j].1;
                            if child.leaf || last {
                                return overlap(p.to_string());
                            }
                            child
                        }
                        None => {
                            node.fields.push((f.clone(), Node::default()));
                            &mut node.fields.last_mut().expect("just pushed").1
                        }
                    }
                }
                Step::Index(x) => {
                    if !node.fields.is_empty() {
                        return conflict(p.to_string());
                    }
                    match node.indexes.iter().position(|(n, _)| n == x) {
                        Some(j) => {
                            let child = &mut node.indexes[j].1;
                            if child.leaf || last {
                                return overlap(p.to_string());
                            }
                            child
                        }
                        None => {
                            node.indexes.push((*x, Node::default()));
                            &mut node.indexes.last_mut().expect("just pushed").1
                        }
                    }
                }
            };
        }
        node.leaf = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::parse::parse_projection;

    fn check(src: &str) -> Option<String> {
        let paths = parse_projection(src).unwrap();
        let mut b = Bindings {
            names: None,
            values: None,
            used_names: HashSet::new(),
            used_values: HashSet::new(),
        };
        let mut set = PathSet::default();
        for p in &paths {
            let r = b.path(p).unwrap();
            if let Err((overlap, at)) = set.insert(&r) {
                return Some(format!(
                    "{} at {at}",
                    if overlap { "overlap" } else { "conflict" }
                ));
            }
        }
        None
    }

    #[test]
    fn overlaps_and_conflicts_match_scylla() {
        let cases = [
            ("x, x", "overlap at x"),
            ("m, m.q", "overlap at m"),
            ("a.b, a", "overlap at a"),
            ("a.b, a.b[0]", "overlap at a.b[0]"),
            ("a.b[0], a.b[0]", "overlap at a.b[0]"),
            ("a.b[1].c, a.b[1]", "overlap at a.b[1]"),
            ("a.b[0], a.b.c", "conflict at a.b.c"),
            ("a.b.c, a.b[0]", "conflict at a.b[0]"),
            ("l[0], l.x", "conflict at l.x"),
            ("l[2][0], l[2].z", "conflict at l[2].z"),
            ("a.b[0], a.b[1], a", "overlap at a"),
            ("doc.a.b, doc.a", "overlap at doc.a"),
            ("doc.a, doc[1]", "conflict at doc[1]"),
            ("a.b, a.b.c", "overlap at a.b.c"),
            ("a.b.c.d.e, a.b", "overlap at a.b"),
            ("a[1][1], a[1]", "overlap at a[1]"),
            ("a[1], a[1][1]", "overlap at a[1][1]"),
            ("a, a[1]", "overlap at a"),
        ];
        for (src, want) in cases {
            assert_eq!(check(src).as_deref(), Some(want), "{src}");
        }
        assert_eq!(check("a.b, a.c, b[0], b[1], c"), None);
    }
}
