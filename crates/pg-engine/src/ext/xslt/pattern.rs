//! XSLT match patterns (libxslt's pattern.c): compiling, default priorities, matching.

use super::compile::{Compiler, search_ns};
use super::style::{Expr, QName};
use crate::types::xml::tree::{Kind, NodeId};
use crate::types::xml::xpath::{XDoc, XNode};

#[derive(Clone)]
pub enum Test {
    /// A QName (or `*` with `local` "*" and a URI for `p:*`).
    Name(Option<String>, String),
    NsAll(Option<String>),
    All,
    Node,
    Text,
    Comment,
    Pi(Option<String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rel {
    Parent,
    Ancestor,
}

#[derive(Clone)]
pub struct Step {
    pub attr: bool,
    pub test: Test,
    pub preds: Vec<Expr>,
    /// How the step to its left relates: `/` or `//`.
    pub rel: Rel,
}

#[derive(Clone)]
pub enum Anchor {
    None,
    Root,
    Id(String),
    Key(QName, String),
}

#[derive(Clone)]
pub struct Alt {
    /// Right to left: steps[0] tests the node itself.
    pub steps: Vec<Step>,
    pub anchor: Anchor,
    pub priority: f32,
}

#[derive(Clone)]
pub struct Pattern {
    pub alts: Vec<Alt>,
}

/// What matching needs from the transformation: predicates, id() and key().
pub trait Env {
    fn predicate(&self, e: &Expr, node: &XNode, pos: i32, size: i32) -> Result<bool, ()>;
    fn id_nodes(&self, node: &XNode, ids: &str) -> Vec<XNode>;
    fn key_nodes(&self, node: &XNode, name: &QName, value: &str) -> Result<Vec<XNode>, ()>;
}

struct P<'a, 'c, 'd> {
    c: &'c mut Compiler<'d>,
    id: NodeId,
    s: &'a str,
    i: usize,
}

/// `xsltCompilePattern`: Err (an error counted) when the pattern doesn't compile.
pub(super) fn compile(c: &mut Compiler, id: NodeId, s: &str) -> Result<Pattern, ()> {
    let mut p = P { c, id, s, i: 0 };
    let mut alts = Vec::new();
    loop {
        p.ws();
        match p.alt() {
            Some(a) => alts.push(a),
            None => {
                p.c.errors += 1;
                return Err(());
            }
        }
        p.ws();
        if p.peek() == Some('|') {
            p.i += 1;
            continue;
        }
        if p.i < p.s.len() || alts.is_empty() {
            p.c.errors += 1;
            return Err(());
        }
        return Ok(Pattern { alts });
    }
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '\u{B7}')
}

impl P<'_, '_, '_> {
    fn peek(&self) -> Option<char> {
        self.s[self.i..].chars().next()
    }

    fn starts(&self, t: &str) -> bool {
        self.s[self.i..].starts_with(t)
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.i += 1;
        }
    }

    fn ncname(&mut self) -> Option<String> {
        let start = self.i;
        match self.peek() {
            Some(c) if is_name_start(c) => self.i += c.len_utf8(),
            _ => return None,
        }
        while let Some(c) = self.peek() {
            if !is_name_char(c) {
                break;
            }
            self.i += c.len_utf8();
        }
        Some(self.s[start..self.i].to_string())
    }

    fn literal(&mut self) -> Option<String> {
        let q = self.peek()?;
        if q != '\'' && q != '"' {
            return None;
        }
        let end = self.s[self.i + 1..].find(q)? + self.i + 1;
        let v = self.s[self.i + 1..end].to_string();
        self.i = end + 1;
        Some(v)
    }

    fn uri(&mut self, prefix: &str) -> Option<Option<String>> {
        match search_ns(&self.c.doc, self.id, Some(prefix)) {
            Some(ns) => Some(ns.href),
            None => None,
        }
    }

    fn alt(&mut self) -> Option<Alt> {
        let mut anchor = Anchor::None;
        let mut first_rel = None;
        let mut explicit = None;
        if self.starts("//") {
            self.i += 2;
            first_rel = Some(Rel::Ancestor);
            anchor = Anchor::Root;
            explicit = Some(0.5);
        } else if self.starts("/") {
            self.i += 1;
            self.ws();
            anchor = Anchor::Root;
            first_rel = Some(Rel::Parent);
            match self.peek() {
                None | Some('|') => return Some(Alt { steps: Vec::new(), anchor, priority: 0.5 }),
                _ => {}
            }
        } else if self.starts("id") || self.starts("key") {
            let save = self.i;
            let f = self.ncname()?;
            self.ws();
            if self.peek() == Some('(') && (f == "id" || f == "key") {
                self.i += 1;
                self.ws();
                let a1 = self.literal()?;
                self.ws();
                if f == "key" {
                    if self.peek() != Some(',') {
                        return None;
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.literal()?;
                    self.ws();
                    let name = self.c.qname(self.id, &a1)?;
                    anchor = Anchor::Key(name, v);
                } else {
                    anchor = Anchor::Id(a1);
                }
                if self.peek() != Some(')') {
                    return None;
                }
                self.i += 1;
                self.ws();
                explicit = Some(0.5);
                if self.starts("//") {
                    self.i += 2;
                    first_rel = Some(Rel::Ancestor);
                } else if self.starts("/") {
                    self.i += 1;
                    first_rel = Some(Rel::Parent);
                } else {
                    return Some(Alt { steps: Vec::new(), anchor, priority: 0.5 });
                }
            } else {
                self.i = save;
            }
        }
        let mut steps = Vec::new();
        let mut rel = first_rel.unwrap_or(Rel::Parent);
        loop {
            self.ws();
            let mut st = self.step()?;
            st.rel = rel;
            steps.push(st);
            self.ws();
            if self.starts("//") {
                self.i += 2;
                rel = Rel::Ancestor;
            } else if self.starts("/") {
                self.i += 1;
                rel = Rel::Parent;
            } else {
                break;
            }
        }
        // Right to left; each step's `rel` is the separator to its left.
        steps.reverse();
        let priority = explicit.unwrap_or_else(|| default_priority(&steps, &anchor));
        Some(Alt { steps, anchor, priority })
    }

    fn step(&mut self) -> Option<Step> {
        let mut attr = false;
        if self.peek() == Some('@') {
            self.i += 1;
            attr = true;
        } else if self.starts("attribute") || self.starts("child") {
            let save = self.i;
            let a = self.ncname()?;
            self.ws();
            if self.starts("::") {
                self.i += 2;
                self.ws();
                attr = a == "attribute";
            } else {
                self.i = save;
            }
        }
        self.ws();
        let test = self.node_test(attr)?;
        let mut preds = Vec::new();
        loop {
            self.ws();
            if self.peek() != Some('[') {
                break;
            }
            let start = self.i + 1;
            let end = self.bracket_end(start)?;
            let e = self.c.xpath_novar(self.id, &self.s[start..end])?;
            preds.push(e);
            self.i = end + 1;
        }
        Some(Step { attr, test, preds, rel: Rel::Parent })
    }

    fn bracket_end(&self, start: usize) -> Option<usize> {
        let b = self.s.as_bytes();
        let (mut depth, mut i) = (0, start);
        while i < b.len() {
            match b[i] {
                b'\'' | b'"' => {
                    let q = b[i];
                    i += 1;
                    while i < b.len() && b[i] != q {
                        i += 1;
                    }
                }
                b'[' => depth += 1,
                b']' if depth == 0 => return Some(i),
                b']' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        None
    }

    fn node_test(&mut self, attr: bool) -> Option<Test> {
        if self.peek() == Some('*') {
            self.i += 1;
            return Some(Test::All);
        }
        let name = self.ncname()?;
        self.ws();
        if self.peek() == Some('(') && matches!(name.as_str(), "node" | "text" | "comment" | "processing-instruction") {
            self.i += 1;
            self.ws();
            let lit = if name == "processing-instruction" { self.literal() } else { None };
            self.ws();
            if self.peek() != Some(')') {
                return None;
            }
            self.i += 1;
            return Some(match name.as_str() {
                "node" => Test::Node,
                "text" => Test::Text,
                "comment" => Test::Comment,
                _ => Test::Pi(lit),
            });
        }
        if self.peek() == Some(':') && !self.starts("::") {
            self.i += 1;
            let uri = self.uri(&name)?;
            if self.peek() == Some('*') {
                self.i += 1;
                return Some(Test::NsAll(uri));
            }
            let local = self.ncname()?;
            return Some(Test::Name(uri, local));
        }
        let _ = attr;
        Some(Test::Name(None, name))
    }
}

/// The default priority libxslt gives an alternative (xsltCompilePattern).
fn default_priority(steps: &[Step], anchor: &Anchor) -> f32 {
    if steps.len() != 1 || !matches!(anchor, Anchor::None) || !steps[0].preds.is_empty() {
        return 0.5;
    }
    match &steps[0].test {
        Test::Name(..) => 0.0,
        Test::Pi(Some(_)) => 0.0,
        Test::NsAll(_) => -0.25,
        Test::All if steps[0].attr => -0.5,
        Test::All | Test::Node | Test::Text | Test::Comment | Test::Pi(None) => -0.5,
    }
}

impl Pattern {
    /// Whether `node` matches some alternative.
    pub fn matches(&self, x: &XDoc, node: &XNode, env: &dyn Env) -> Result<bool, ()> {
        for a in &self.alts {
            if a.matches(x, node, env)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Alt {
    pub fn matches(&self, x: &XDoc, node: &XNode, env: &dyn Env) -> Result<bool, ()> {
        if self.steps.is_empty() {
            return Ok(match &self.anchor {
                Anchor::Root => matches!(node, XNode::Node(id) if x.doc.nodes[*id].kind == Kind::Document),
                Anchor::Id(ids) => contains(&env.id_nodes(node, ids), node),
                Anchor::Key(n, v) => contains(&env.key_nodes(node, n, v)?, node),
                Anchor::None => false,
            });
        }
        self.match_from(x, node, 0, env)
    }

    fn match_from(&self, x: &XDoc, node: &XNode, k: usize, env: &dyn Env) -> Result<bool, ()> {
        let st = &self.steps[k];
        if !st.matches(x, node, env)? {
            return Ok(false);
        }
        let XNode::Node(id) = node else { return Ok(false) };
        let last = k + 1 == self.steps.len();
        if last {
            return self.anchored(x, *id, st.rel, env);
        }
        match st.rel {
            Rel::Parent => match x.parent(*id) {
                Some(p) => self.match_from(x, &XNode::Node(p), k + 1, env),
                None => Ok(false),
            },
            Rel::Ancestor => {
                let mut cur = x.parent(*id);
                while let Some(p) = cur {
                    if self.match_from(x, &XNode::Node(p), k + 1, env)? {
                        return Ok(true);
                    }
                    cur = x.parent(p);
                }
                Ok(false)
            }
        }
    }

    /// The leftmost step matched at `id`: the anchor, through `rel`.
    fn anchored(&self, x: &XDoc, id: NodeId, rel: Rel, env: &dyn Env) -> Result<bool, ()> {
        let ok = |n: NodeId| -> Result<bool, ()> {
            Ok(match &self.anchor {
                Anchor::None => true,
                Anchor::Root => x.doc.nodes[n].kind == Kind::Document,
                Anchor::Id(ids) => contains(&env.id_nodes(&XNode::Node(n), ids), &XNode::Node(n)),
                Anchor::Key(name, v) => contains(&env.key_nodes(&XNode::Node(n), name, v)?, &XNode::Node(n)),
            })
        };
        if matches!(self.anchor, Anchor::None) {
            return Ok(true);
        }
        match rel {
            Rel::Parent => match x.parent(id) {
                Some(p) => ok(p),
                None => Ok(false),
            },
            Rel::Ancestor => {
                let mut cur = x.parent(id);
                while let Some(p) = cur {
                    if ok(p)? {
                        return Ok(true);
                    }
                    cur = x.parent(p);
                }
                Ok(false)
            }
        }
    }
}

fn contains(set: &[XNode], n: &XNode) -> bool {
    set.iter().any(|m| matches!((m, n), (XNode::Node(a), XNode::Node(b)) if a == b))
}

impl Step {
    fn matches(&self, x: &XDoc, node: &XNode, env: &dyn Env) -> Result<bool, ()> {
        if !self.test.matches(x, node, self.attr) {
            return Ok(false);
        }
        if self.preds.is_empty() {
            return Ok(true);
        }
        let XNode::Node(id) = node else { return Ok(false) };
        // The candidates the predicates count: the node's siblings on the step's axis that
        // pass the node test, then those each predicate keeps.
        let mut set: Vec<XNode> = match x.parent(*id) {
            Some(p) => {
                let pn = &x.doc.nodes[p];
                let sibs = if self.attr { &pn.attrs } else { &pn.children };
                sibs.iter().map(|&s| XNode::Node(s)).filter(|s| self.test.matches(x, s, self.attr)).collect()
            }
            None => vec![node.clone()],
        };
        for e in &self.preds {
            let size = set.len() as i32;
            let mut kept = Vec::new();
            for (i, n) in set.iter().enumerate() {
                if env.predicate(e, n, i as i32 + 1, size)? {
                    kept.push(n.clone());
                }
            }
            set = kept;
        }
        Ok(contains(&set, node))
    }
}

impl Test {
    fn matches(&self, x: &XDoc, n: &XNode, attr: bool) -> bool {
        let XNode::Node(id) = n else { return false };
        let node = &x.doc.nodes[*id];
        let uri = node.ns.as_ref().and_then(|ns| ns.href.as_deref());
        let principal = if attr { Kind::Attribute } else { Kind::Element };
        match self {
            Test::Name(u, l) => node.kind == principal && &node.name == l && u.as_deref() == uri,
            Test::NsAll(u) => node.kind == principal && u.as_deref() == uri,
            Test::All => node.kind == principal,
            _ if attr => false,
            Test::Node => matches!(node.kind, Kind::Element | Kind::Text | Kind::CData | Kind::Comment | Kind::Pi),
            Test::Text => matches!(node.kind, Kind::Text | Kind::CData),
            Test::Comment => node.kind == Kind::Comment,
            Test::Pi(t) => node.kind == Kind::Pi && t.as_ref().is_none_or(|t| *t == node.name),
        }
    }
}
