//! JSONPath like RedisJSON 8.10: the same grammar (so the same syntax errors), the same
//! selection order, filters with RFC 9535 functions, and computed *projections* (`$.a + 1`,
//! `$.arr.length()`), which only reads evaluate.
//!
//! The parse tree is interpreted directly. Values are [`Node`]s: borrowed document values, or
//! numbers read out of typed arrays — which, having no place in the document, never equal a
//! borrowed value in `==` between two nodes (as in RedisJSON, where they are owned copies).

use super::ser::plain;
use super::value::{Arr, J, N, Node, Obj};
use pest::Parser;
use pest::iterators::{Pair, Pairs};
use regex::Regex;
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(pest_derive::Parser)]
#[grammar = "json/path.pest"]
struct Grammar;

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Rule::literal => f.write_str("<string>"),
            Rule::all => f.write_str("'*'"),
            Rule::full_scan => f.write_str("'..'"),
            Rule::numbers_list => f.write_str("'<number>[,<number>,...]'"),
            Rule::string_list => f.write_str("'<string>[,<string>,...]'"),
            Rule::numbers_range => f.write_str("['start:end:steps']"),
            Rule::number => f.write_str("'<number>'"),
            Rule::filter => f.write_str("'[?(filter_expression)]'"),
            _ => write!(f, "{self:?}"),
        }
    }
}

/// A step of a matched value's location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Key(String),
    Index(usize),
}

impl Step {
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            Step::Key(k) => Cow::Borrowed(k),
            Step::Index(i) => Cow::Owned(i.to_string()),
        }
    }
}

/// The last step of a static path, for creating a missing member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Last {
    Key(String),
    Index(String),
}

/// A compiled path.
#[derive(Clone, Debug)]
pub struct Query<'i> {
    root: Vec<Pair<'i, Rule>>,
    projection: Option<Pair<'i, Rule>>,
}

impl<'i> Query<'i> {
    pub fn is_projection(&self) -> bool {
        self.projection.is_some()
    }

    /// At most one match: every step a name or a single index.
    pub fn is_static(&self) -> bool {
        self.root.iter().all(|p| match p.as_rule() {
            Rule::literal | Rule::number => true,
            Rule::numbers_list | Rule::string_list => p.clone().into_inner().count() <= 1,
            _ => false,
        })
    }

    pub fn size(&self) -> usize {
        self.root.len()
    }

    /// Remove the last step, if it is a name or an index.
    pub fn pop_last(&mut self) -> Option<Last> {
        let last = self.root.pop()?;
        match last.as_rule() {
            Rule::literal => Some(Last::Key(last.as_str().to_string())),
            Rule::number => Some(Last::Index(last.as_str().to_string())),
            Rule::numbers_list => last.into_inner().next().map(|p| Last::Index(p.as_str().to_string())),
            Rule::string_list => last.into_inner().next().map(|p| Last::Key(unescape(&p).into_owned())),
            _ => None,
        }
    }
}

/// Bracket and parenthesis nesting outside quoted strings.
fn max_bracket_depth(path: &str) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for &b in path.as_bytes() {
        match quote {
            Some(q) => match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                _ if b == q => quote = None,
                _ => {}
            },
            None => match b {
                b'\'' | b'"' => quote = Some(b),
                b'(' | b'[' => {
                    depth += 1;
                    max = max.max(depth);
                }
                b')' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            },
        }
    }
    max
}

pub fn compile(path: &str) -> Result<Query<'_>, String> {
    const MAX_NESTING: usize = 128;
    if max_bracket_depth(path) > MAX_NESTING {
        return Err(format!("Error occurred at position 0, JSONPath nesting too deep (max depth {MAX_NESTING})"));
    }
    match Grammar::parse(Rule::query, path) {
        Ok(mut q) => {
            let expr = q.next().ok_or("Error occurred at position 0, internal: empty JSONPath parse result")?;
            match classify(&expr) {
                Some(root) => Ok(Query { root, projection: None }),
                None => Ok(Query { root: Vec::new(), projection: Some(expr) }),
            }
        }
        Err(e) => {
            let at = match e.location {
                pest::error::InputLocation::Pos(p) => p,
                pest::error::InputLocation::Span((p, _)) => p,
            };
            let msg = match &e.variant {
                pest::error::ErrorVariant::ParsingError { positives, negatives } => {
                    let join = |v: &[Rule]| v.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(", ");
                    let (p, n) = (join(positives), join(negatives));
                    match (p.is_empty(), n.is_empty()) {
                        (true, true) => "parsing error".to_string(),
                        (false, true) => format!("expected one of the following: {p}"),
                        (true, false) => format!("unexpected tokens found: {n}"),
                        (false, false) => format!("expected one of the following: {p}, unexpected tokens found: {n}"),
                    }
                }
                pest::error::ErrorVariant::CustomError { message } => message.clone(),
            };
            Err(format!("Error occurred at position {at}, Error at position {at}: {msg}"))
        }
    }
}

/// The steps of a lone `$…` (possibly parenthesized); `None` for a projection.
fn classify<'i>(expr: &Pair<'i, Rule>) -> Option<Vec<Pair<'i, Rule>>> {
    let mut terms = expr.clone().into_inner();
    let term = terms.next()?;
    if terms.next().is_some() {
        return None;
    }
    let mut factors = term.into_inner();
    let factor = factors.next()?;
    if factors.next().is_some() {
        return None;
    }
    let prim = factor.into_inner().next()?;
    match prim.as_rule() {
        Rule::from_root => Some(prim.into_inner().next().map(|r| r.into_inner().collect()).unwrap_or_default()),
        Rule::arith_expr => classify(&prim),
        _ => None,
    }
}

fn unescape<'a>(p: &Pair<'a, Rule>) -> Cow<'a, str> {
    let s = p.as_str();
    match p.as_rule() {
        Rule::string_value_escape_1 => Cow::Owned(s.replace("\\\\", "\\").replace("\\\"", "\"")),
        Rule::string_value_escape_2 => Cow::Owned(s.replace("\\\\", "\\").replace("\\'", "'")),
        _ => Cow::Borrowed(s),
    }
}

/// How a value presents itself to paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Null,
    Bool,
    Long,
    Double,
    String,
    Array,
    Object,
}

impl<'a> Node<'a> {
    pub fn ty(&self) -> Ty {
        match self {
            Node::Num(N::I(_)) => Ty::Long,
            Node::Num(_) => Ty::Double,
            Node::Ref(j) => match j {
                J::Null => Ty::Null,
                J::Bool(_) => Ty::Bool,
                J::Num(N::I(_)) => Ty::Long,
                J::Num(_) => Ty::Double,
                J::Str(_) => Ty::String,
                J::Arr(_) => Ty::Array,
                J::Obj(_) => Ty::Object,
            },
        }
    }

    pub fn num(&self) -> Option<N> {
        match self {
            Node::Num(n) => Some(*n),
            Node::Ref(J::Num(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn long(&self) -> Option<i64> {
        match self.num()? {
            N::I(v) => Some(v),
            _ => None,
        }
    }

    pub fn double(&self) -> Option<f64> {
        self.num().map(N::to_f64_lossy)
    }

    pub fn as_str(&self) -> Option<&'a str> {
        match self {
            Node::Ref(J::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn bool(&self) -> Option<bool> {
        match self {
            Node::Ref(J::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn arr(&self) -> Option<&'a Arr> {
        match self {
            Node::Ref(J::Arr(a)) => Some(a),
            _ => None,
        }
    }

    pub fn obj(&self) -> Option<&'a Obj> {
        match self {
            Node::Ref(J::Obj(o)) => Some(o),
            _ => None,
        }
    }

    pub fn child_count(&self) -> Option<usize> {
        self.arr().map(Arr::len).or_else(|| self.obj().map(Obj::len))
    }

    /// Array elements or object member values.
    pub fn values(&self) -> Vec<Node<'a>> {
        if let Some(a) = self.arr() {
            a.iter().collect()
        } else if let Some(o) = self.obj() {
            o.values().map(Node::Ref).collect()
        } else {
            Vec::new()
        }
    }

    fn key(&self, k: &str) -> Option<Node<'a>> {
        self.obj().and_then(|o| o.get(k)).map(Node::Ref)
    }

    fn index(&self, i: usize) -> Option<Node<'a>> {
        self.arr().and_then(|a| a.get(i))
    }

    /// `ValueRef == ValueRef`: two document values by value (arrays only of the same element
    /// type), two typed-array numbers by value, never one of each.
    fn same_ref(&self, other: &Node) -> bool {
        match (self, other) {
            (Node::Ref(a), Node::Ref(b)) => a.same(b),
            (Node::Num(a), Node::Num(b)) => a.same(*b),
            _ => false,
        }
    }
}

/// Deep equality by presented type: a Long never equals a Double.
pub fn is_equal(a: Node, b: Node) -> bool {
    a.ty() == b.ty()
        && match a.ty() {
            Ty::Null => true,
            Ty::Bool => a.bool() == b.bool(),
            Ty::Long => a.long() == b.long(),
            Ty::Double => a.double() == b.double(),
            Ty::String => a.as_str() == b.as_str(),
            Ty::Array => {
                let (x, y) = (a.values(), b.values());
                x.len() == y.len() && x.into_iter().zip(y).all(|(x, y)| is_equal(x, y))
            }
            Ty::Object => {
                let (Some(x), Some(y)) = (a.obj(), b.obj()) else { return false };
                x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| is_equal(Node::Ref(v), Node::Ref(w))))
            }
        }
}

#[derive(Clone, Copy)]
enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
}

fn numbers_equal(a: Num, b: Num) -> bool {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => x == y,
        (Num::Float(x), Num::Float(y)) => x == y,
        (Num::Int(x), Num::Float(y)) | (Num::Float(y), Num::Int(x)) => x as f64 == y,
    }
}

fn node_number(v: Node) -> Option<Num> {
    match v.ty() {
        Ty::Long => v.long().map(Num::Int),
        Ty::Double => v.double().map(Num::Float),
        _ => None,
    }
}

fn values_equal(a: Node, b: Node) -> bool {
    match (node_number(a), node_number(b)) {
        (Some(x), Some(y)) => numbers_equal(x, y),
        _ => is_equal(a, b),
    }
}

/// A filter or projection operand.
#[derive(Clone, Debug)]
enum T<'i, 'j> {
    Integer(i64),
    Float(f64),
    Str(Cow<'i, str>),
    Value(Node<'j>),
    /// A JSON literal, or a computed value standing alone.
    Literal(Rc<J>),
    Bool(bool),
    Null,
    /// Several matches: comparisons hold if any of them does.
    NodeList(Vec<Node<'j>>),
    /// Computed values (`keys()`, `~`, `append()`).
    Results(Vec<J>),
    Invalid,
}

fn literal_term<'i, 'j>(v: &J) -> T<'i, 'j> {
    match v {
        J::Num(N::I(i)) => T::Integer(*i),
        J::Num(n) => T::Float(n.to_f64_lossy()),
        J::Str(s) => T::Str(Cow::Owned(s.to_string())),
        J::Bool(b) => T::Bool(*b),
        J::Null => T::Null,
        other => T::Literal(Rc::new(other.clone())),
    }
}

/// Is `needle` an element of the array-shaped `hay`.
fn value_in_array(needle: Node, hay: &T) -> bool {
    match hay {
        T::Value(v) if v.ty() == Ty::Array => v.values().into_iter().any(|e| values_equal(needle, e)),
        T::Literal(l) => match &**l {
            J::Arr(a) => a.iter().any(|e| values_equal(needle, e)),
            _ => false,
        },
        T::NodeList(list) => list.iter().any(|v| values_equal(needle, *v)),
        T::Results(vs) => vs.iter().any(|v| values_equal(needle, Node::Ref(v))),
        _ => false,
    }
}

fn num_binop<'i, 'j>(oper: Rule, a: Num, b: Num) -> Option<T<'i, 'j>> {
    match oper {
        Rule::add | Rule::sub | Rule::mul => {
            if let (Num::Int(x), Num::Int(y)) = (a, b) {
                let r = match oper {
                    Rule::add => x.checked_add(y),
                    Rule::sub => x.checked_sub(y),
                    _ => x.checked_mul(y),
                };
                if let Some(v) = r {
                    return Some(T::Integer(v));
                }
            }
            let (x, y) = (a.f64(), b.f64());
            Some(T::Float(match oper {
                Rule::add => x + y,
                Rule::sub => x - y,
                _ => x * y,
            }))
        }
        Rule::div => {
            let y = b.f64();
            (y != 0.0).then(|| T::Float(a.f64() / y))
        }
        Rule::rem => match (a, b) {
            (Num::Int(x), Num::Int(y)) => x.checked_rem(y).map(T::Integer),
            _ => {
                let y = b.f64();
                (y != 0.0).then(|| T::Float(a.f64() % y))
            }
        },
        _ => None,
    }
}

fn f64_to_i64(v: f64) -> Option<i64> {
    (v.is_finite() && v >= i64::MIN as f64 && v < i64::MAX as f64).then_some(v as i64)
}

fn str_len(v: Node) -> Option<usize> {
    match v.ty() {
        Ty::String => v.as_str().map(|s| s.chars().count()),
        Ty::Array | Ty::Object => v.child_count(),
        _ => None,
    }
}

impl<'i, 'j> T<'i, 'j> {
    fn as_number(&self) -> Option<Num> {
        match self {
            T::Integer(n) => Some(Num::Int(*n)),
            T::Float(f) => Some(Num::Float(*f)),
            T::Value(v) => node_number(*v),
            T::Literal(l) => node_number(Node::Ref(l)),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            T::Str(s) => Some(s),
            T::Value(v) => v.as_str(),
            T::Literal(l) => match &**l {
                J::Str(s) => Some(s),
                _ => None,
            },
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            T::Bool(b) => Some(*b),
            T::Literal(l) => match &**l {
                J::Bool(b) => Some(*b),
                _ => None,
            },
            T::Value(v) => v.bool(),
            _ => None,
        }
    }

    fn cmp(&self, s: &T) -> Option<Ordering> {
        match (self, s) {
            (T::Integer(a), T::Integer(b)) => Some(a.cmp(b)),
            (T::Float(_), T::Integer(b)) => self.cmp(&T::Float(*b as f64)),
            (T::Integer(a), T::Float(_)) => T::Float(*a as f64).cmp(s),
            (T::Float(a), T::Float(b)) => Some(if a > b {
                Ordering::Greater
            } else if a < b {
                Ordering::Less
            } else {
                Ordering::Equal
            }),
            (T::Str(a), T::Str(b)) => Some((**a).cmp(&**b)),
            (T::Bool(a), T::Bool(b)) => Some(a.cmp(b)),
            (T::Null, T::Null) => Some(Ordering::Equal),
            (T::Value(v), _) => T::scalar(*v)?.cmp(s),
            (_, T::Value(v)) => self.cmp(&T::scalar(*v)?),
            _ => None,
        }
    }

    /// A scalar node as an operand.
    fn scalar(v: Node<'j>) -> Option<T<'i, 'j>> {
        Some(match v.ty() {
            Ty::Long => T::Integer(v.long()?),
            Ty::Double => T::Float(v.double()?),
            Ty::String => T::Str(Cow::Owned(v.as_str()?.to_string())),
            Ty::Bool => T::Bool(v.bool()?),
            Ty::Null => T::Null,
            _ => return None,
        })
    }

    fn ord(&self, s: &T<'i, 'j>, pred: fn(Ordering) -> bool) -> bool {
        match (self, s) {
            (T::NodeList(list), _) => list.iter().any(|v| T::Value(*v).ord(s, pred)),
            (_, T::NodeList(list)) => list.iter().any(|v| self.ord(&T::Value(*v), pred)),
            (T::Results(vs), _) => vs.iter().any(|v| literal_term(v).ord(s, pred)),
            (_, T::Results(vs)) => vs.iter().any(|v| self.ord(&literal_term(v), pred)),
            _ => self.cmp(s).is_some_and(pred),
        }
    }

    fn eq(&self, s: &T<'i, 'j>) -> bool {
        match (self, s) {
            (T::NodeList(list), _) => list.iter().any(|v| T::Value(*v).eq(s)),
            (_, T::NodeList(list)) => list.iter().any(|v| self.eq(&T::Value(*v))),
            (T::Results(vs), _) => vs.iter().any(|v| literal_term(v).eq(s)),
            (_, T::Results(vs)) => vs.iter().any(|v| self.eq(&literal_term(v))),
            (T::Value(a), T::Value(b)) => a.same_ref(b),
            (T::Value(v), T::Literal(l)) | (T::Literal(l), T::Value(v)) => is_equal(*v, Node::Ref(l)),
            (T::Literal(a), T::Literal(b)) => is_equal(Node::Ref(a), Node::Ref(b)),
            _ => self.cmp(s) == Some(Ordering::Equal),
        }
    }

    fn equals_value(&self, other: Node) -> bool {
        if let (Some(a), Some(b)) = (self.as_number(), node_number(other)) {
            return numbers_equal(a, b);
        }
        match self {
            T::Value(v) => is_equal(*v, other),
            T::Literal(l) => is_equal(Node::Ref(l), other),
            T::NodeList(list) => list.iter().any(|v| is_equal(*v, other)),
            T::Str(s) => other.as_str() == Some(&**s),
            T::Bool(b) => other.bool() == Some(*b),
            T::Null => other.ty() == Ty::Null,
            _ => false,
        }
    }

    fn member_of(&self, arr: &T) -> bool {
        match arr {
            T::Value(v) if v.ty() == Ty::Array => v.values().into_iter().any(|e| self.equals_value(e)),
            T::Literal(l) => match &**l {
                J::Arr(a) => a.iter().any(|e| self.equals_value(e)),
                _ => false,
            },
            T::NodeList(list) => list.iter().any(|v| self.equals_value(*v)),
            T::Results(vs) => vs.iter().any(|v| self.equals_value(Node::Ref(v))),
            _ => false,
        }
    }

    fn set_relate(&self, rhs: &T, all: bool) -> bool {
        let combine = |mut it: Box<dyn Iterator<Item = bool> + '_>| if all { it.all(|m| m) } else { it.any(|m| m) };
        match self {
            T::NodeList(list) => list.iter().any(|v| T::Value(*v).set_relate(rhs, all)),
            T::Value(v) if v.ty() == Ty::Array => combine(Box::new(v.values().into_iter().map(|e| value_in_array(e, rhs)))),
            T::Literal(l) => match &**l {
                J::Arr(a) => combine(Box::new(a.iter().map(|e| value_in_array(e, rhs)))),
                _ => false,
            },
            T::Results(vs) => combine(Box::new(vs.iter().map(|v| value_in_array(Node::Ref(v), rhs)))),
            _ => false,
        }
    }

    fn seq_length(&self) -> Option<usize> {
        match self {
            T::Str(s) => Some(s.chars().count()),
            T::Value(v) => str_len(*v),
            T::Literal(l) => str_len(Node::Ref(l)),
            T::Results(vs) => Some(vs.len()),
            _ => None,
        }
    }

    fn size_of(&self, rhs: &T<'i, 'j>) -> bool {
        if let T::NodeList(list) = self {
            return list.iter().any(|v| T::Value(*v).size_of(rhs));
        }
        if let T::NodeList(list) = rhs {
            return list.iter().any(|v| self.size_of(&T::Value(*v)));
        }
        let target = match rhs.as_number() {
            Some(Num::Int(n)) => n,
            Some(Num::Float(f)) => match f64_to_i64(f.trunc()) {
                Some(n) => n,
                None => return false,
            },
            None => return false,
        };
        target >= 0 && self.seq_length().is_some_and(|l| l as i64 == target)
    }

    fn empty_check(&self, rhs: &T<'i, 'j>) -> bool {
        if let T::NodeList(list) = self {
            return list.iter().any(|v| T::Value(*v).empty_check(rhs));
        }
        if let T::NodeList(list) = rhs {
            return list.iter().any(|v| self.empty_check(&T::Value(*v)));
        }
        let (Some(len), Some(want)) = (self.seq_length(), rhs.as_bool()) else { return false };
        (len == 0) == want
    }

    fn re(&self, s: &T<'i, 'j>, cache: &mut RegexCache) -> bool {
        match (self, s) {
            (T::NodeList(list), _) => list.iter().any(|v| T::Value(*v).re(s, cache)),
            (_, T::NodeList(list)) => list.iter().any(|v| self.re(&T::Value(*v), cache)),
            _ => match (self.as_str(), s.as_str()) {
                (Some(subject), Some(re)) => regex_matches(cache, re, false, subject),
                _ => false,
            },
        }
    }
}

type RegexCache = HashMap<String, Option<Regex>>;

fn regex_matches(cache: &mut RegexCache, pattern: &str, full: bool, s: &str) -> bool {
    const MAX: usize = 64;
    let key = if full { format!("^(?:{pattern})$") } else { pattern.to_string() };
    if cache.len() < MAX || cache.contains_key(&key) {
        cache.entry(key).or_insert_with_key(|k| Regex::new(k).ok()).as_ref().is_some_and(|re| re.is_match(s))
    } else {
        Regex::new(&key).is_ok_and(|re| re.is_match(s))
    }
}

fn arith_binop<'i, 'j>(oper: Rule, a: &T<'i, 'j>, b: &T<'i, 'j>) -> T<'i, 'j> {
    match (a.as_number(), b.as_number()) {
        (Some(x), Some(y)) => num_binop(oper, x, y).unwrap_or(T::Invalid),
        _ => T::Invalid,
    }
}

fn arith_unary<'i, 'j>(oper: Rule, v: T<'i, 'j>) -> T<'i, 'j> {
    match v.as_number() {
        Some(Num::Int(n)) if oper == Rule::neg => n.checked_neg().map_or(T::Float(-(n as f64)), T::Integer),
        Some(Num::Int(n)) => T::Integer(n),
        Some(Num::Float(f)) if oper == Rule::neg => T::Float(-f),
        Some(Num::Float(f)) => T::Float(f),
        None => T::Invalid,
    }
}

fn number_seq(arg: &T) -> Option<Vec<f64>> {
    let out: Vec<f64> = match arg {
        T::Value(v) if v.ty() == Ty::Array => v.values().into_iter().map(|e| node_number(e).map(Num::f64)).collect::<Option<_>>()?,
        T::Literal(l) => match &**l {
            J::Arr(a) => a.iter().map(|e| node_number(e).map(Num::f64)).collect::<Option<_>>()?,
            _ => return None,
        },
        T::NodeList(list) => list.iter().map(|e| node_number(*e).map(Num::f64)).collect::<Option<_>>()?,
        T::Results(vs) => vs.iter().map(|e| node_number(Node::Ref(e)).map(Num::f64)).collect::<Option<_>>()?,
        _ => return None,
    };
    (!out.is_empty()).then_some(out)
}

fn avg(s: &[f64]) -> f64 {
    s.iter().sum::<f64>() / s.len() as f64
}

fn index_of<'i, 'j>(arg: Option<T<'i, 'j>>, idx: i64) -> T<'i, 'j> {
    fn resolve(idx: i64, len: usize) -> Option<usize> {
        let i = if idx < 0 { idx + len as i64 } else { idx };
        usize::try_from(i).ok().filter(|&u| u < len)
    }
    match arg {
        Some(T::Value(v)) if v.ty() == Ty::Array => match resolve(idx, v.child_count().unwrap_or(0)) {
            Some(i) => v.index(i).map_or(T::Invalid, T::Value),
            None => T::Invalid,
        },
        Some(T::Literal(l)) => match &*l {
            J::Arr(a) => match resolve(idx, a.len()) {
                Some(i) => T::Literal(Rc::new(a.get(i).map(Node::to_j).unwrap_or_default())),
                None => T::Invalid,
            },
            _ => T::Invalid,
        },
        Some(T::NodeList(mut list)) => match resolve(idx, list.len()) {
            Some(i) => T::Value(list.swap_remove(i)),
            None => T::Invalid,
        },
        Some(T::Results(mut vs)) => match resolve(idx, vs.len()) {
            Some(i) => T::Literal(Rc::new(vs.swap_remove(i))),
            None => T::Invalid,
        },
        _ => T::Invalid,
    }
}

fn keys_of<'i, 'j>(arg: Option<&T<'i, 'j>>) -> T<'i, 'j> {
    let names = |v: Node, out: &mut Vec<J>| {
        if let Some(o) = v.obj() {
            out.extend(o.keys().map(J::str));
        }
    };
    match arg {
        Some(T::Value(v)) if v.ty() == Ty::Object => {
            let mut out = Vec::new();
            names(*v, &mut out);
            T::Results(out)
        }
        Some(T::Literal(l)) => match &**l {
            J::Obj(o) => T::Results(o.keys().map(J::str).collect()),
            _ => T::Invalid,
        },
        Some(T::NodeList(list)) => {
            let mut out = Vec::new();
            for v in list {
                names(*v, &mut out);
            }
            T::Results(out)
        }
        _ => T::Invalid,
    }
}

fn append<'i, 'j>(recv: Option<T<'i, 'j>>, x: Option<T<'i, 'j>>) -> T<'i, 'j> {
    let item = match x {
        None | Some(T::Invalid) => return T::Invalid,
        Some(T::NodeList(list)) => J::Arr(Box::new(Arr::het_from(list.into_iter().map(plain).collect()))),
        Some(T::Results(vs)) => J::Arr(Box::new(Arr::het_from(vs))),
        Some(other) => match outputs(other).into_iter().next() {
            Some(v) => v,
            None => return T::Invalid,
        },
    };
    let mut out = match recv {
        Some(T::NodeList(list)) => list.into_iter().map(plain).collect(),
        Some(T::Value(v)) if v.ty() == Ty::Array => v.values().into_iter().map(plain).collect(),
        Some(T::Literal(l)) => match &*l {
            J::Arr(a) => a.iter().map(Node::to_j).collect(),
            other => vec![other.clone()],
        },
        Some(T::Results(vs)) => vs,
        Some(T::Invalid) | None => Vec::new(),
        Some(other) => outputs(other),
    };
    out.push(item);
    T::Results(out)
}

fn eval_function<'i, 'j>(name: &str, mut args: Vec<T<'i, 'j>>, cache: &mut RegexCache) -> T<'i, 'j> {
    let arity_ok = match name {
        "concat" => !args.is_empty(),
        "index" | "append" => args.len() == 2,
        "length" | "count" | "ceiling" | "floor" | "abs" | "sum" | "min" | "max" | "avg" | "stddev" | "first" | "last" | "keys" => args.len() == 1,
        _ => true,
    };
    if !arity_ok {
        return T::Invalid;
    }
    let round = |arg: Option<&T>, f: fn(f64) -> f64| match arg.and_then(T::as_number) {
        Some(Num::Int(n)) => T::Integer(n),
        Some(Num::Float(x)) => f64_to_i64(f(x)).map_or(T::Invalid, T::Integer),
        None => T::Invalid,
    };
    let aggregate = |arg: Option<&T>, f: fn(&[f64]) -> f64| match arg.and_then(number_seq) {
        Some(s) => T::Float(f(&s)),
        None => T::Invalid,
    };
    match name {
        "length" => {
            let n = match &args[0] {
                T::Str(s) => Some(s.chars().count()),
                T::Value(v) => str_len(*v),
                T::Literal(l) => str_len(Node::Ref(l)),
                T::NodeList(list) if list.len() == 1 => str_len(list[0]),
                T::Results(vs) => Some(vs.len()),
                _ => None,
            };
            n.map_or(T::Invalid, |n| T::Integer(n as i64))
        }
        "count" => T::Integer(match &args[0] {
            T::Invalid => 0,
            T::NodeList(list) => list.len() as i64,
            T::Results(vs) => vs.len() as i64,
            _ => 1,
        }),
        "value" if args.len() == 1 => match args.pop().expect("one argument") {
            T::NodeList(mut list) if list.len() == 1 => list.pop().map_or(T::Invalid, T::Value),
            v @ T::Value(_) => v,
            T::Results(mut vs) if vs.len() == 1 => vs.pop().map_or(T::Invalid, |v| T::Literal(Rc::new(v))),
            _ => T::Invalid,
        },
        "ceiling" => round(args.first(), f64::ceil),
        "floor" => round(args.first(), f64::floor),
        "abs" => match args[0].as_number() {
            Some(Num::Int(n)) => n.checked_abs().map_or(T::Invalid, T::Integer),
            Some(Num::Float(f)) => T::Float(f.abs()),
            None => T::Invalid,
        },
        "concat" => {
            let mut out = String::new();
            for a in &args {
                match a.as_str() {
                    Some(s) => out.push_str(s),
                    None => return T::Invalid,
                }
            }
            T::Str(Cow::Owned(out))
        }
        "sum" => aggregate(args.first(), |s| s.iter().sum()),
        "min" => aggregate(args.first(), |s| s.iter().copied().fold(f64::INFINITY, f64::min)),
        "max" => aggregate(args.first(), |s| s.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
        "avg" => aggregate(args.first(), avg),
        "stddev" => aggregate(args.first(), |s| {
            let mean = avg(s);
            (s.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / s.len() as f64).sqrt()
        }),
        "keys" => keys_of(args.first()),
        "append" => {
            let mut it = args.into_iter();
            append(it.next(), it.next())
        }
        "first" => index_of(args.into_iter().next(), 0),
        "last" => index_of(args.into_iter().next(), -1),
        "index" => {
            let mut it = args.into_iter();
            let arr = it.next();
            let idx = it.next().and_then(|a| match a.as_number() {
                Some(Num::Int(n)) => Some(n),
                Some(Num::Float(f)) => f64_to_i64(f.trunc()),
                None => None,
            });
            idx.map_or(T::Invalid, |n| index_of(arr, n))
        }
        "match" | "search" => match (args.first().and_then(T::as_str), args.get(1).and_then(T::as_str)) {
            (Some(s), Some(re)) => T::Bool(regex_matches(cache, re, name == "match", s)),
            _ => T::Bool(false),
        },
        _ => T::Invalid,
    }
}

/// A projection result as output values.
fn outputs(t: T) -> Vec<J> {
    match t {
        T::Integer(n) => vec![J::Num(N::I(n))],
        T::Float(f) => N::float(f).map(J::Num).into_iter().collect(),
        T::Str(s) => vec![J::str(&s)],
        T::Bool(b) => vec![J::Bool(b)],
        T::Null => vec![J::Null],
        T::Literal(v) => vec![Rc::try_unwrap(v).unwrap_or_else(|rc| (*rc).clone())],
        T::Value(v) => vec![plain(v)],
        T::NodeList(list) => vec![J::Arr(Box::new(Arr::het_from(list.into_iter().map(plain).collect())))],
        T::Results(vs) => vs,
        T::Invalid => vec![],
    }
}

/// A match and, when asked for, where it is.
pub struct Found<'j> {
    pub node: Node<'j>,
    pub path: Vec<Step>,
}

struct Calc<'j> {
    root: Node<'j>,
    regex: RefCell<RegexCache>,
    literals: RefCell<HashMap<usize, Rc<J>>>,
}

/// Where results go: the nodes, and their locations when tracked.
struct Out<'j> {
    found: Vec<Found<'j>>,
    track: bool,
}

impl<'j> Calc<'j> {
    fn new(root: Node<'j>) -> Self {
        Calc { root, regex: RefCell::default(), literals: RefCell::default() }
    }

    fn push(out: &mut Out<'j>, node: Node<'j>, path: &[Step]) {
        let path = if out.track { path.to_vec() } else { Vec::new() };
        out.found.push(Found { node, path });
    }

    fn walk<'i>(&self, steps: &[Pair<'i, Rule>], json: Node<'j>, path: &mut Vec<Step>, out: &mut Out<'j>) {
        let Some((curr, rest)) = steps.split_first() else {
            Self::push(out, json, path);
            return;
        };
        match curr.as_rule() {
            Rule::full_scan => {
                self.walk(rest, json, path, out);
                self.full_scan(rest, json, path, out);
            }
            Rule::all => self.each(json, path, &mut |c, v, p| c.walk(rest, v, p, out)),
            Rule::literal => {
                if let Some(v) = json.key(curr.as_str()) {
                    path.push(Step::Key(curr.as_str().to_string()));
                    self.walk(rest, v, path, out);
                    path.pop();
                }
            }
            Rule::string_list => {
                for s in curr.clone().into_inner() {
                    let k = unescape(&s);
                    if let Some(v) = json.key(&k) {
                        path.push(Step::Key(k.into_owned()));
                        self.walk(rest, v, path, out);
                        path.pop();
                    }
                }
            }
            Rule::numbers_list => {
                let Some(n) = json.arr().map(Arr::len) else { return };
                for c in curr.clone().into_inner() {
                    let i = abs_index(parse_index(c.as_str()), n);
                    if let Some(e) = json.index(i) {
                        path.push(Step::Index(i));
                        self.walk(rest, e, path, out);
                        path.pop();
                    }
                }
            }
            Rule::numbers_range => {
                let Some(n) = json.arr().map(Arr::len) else { return };
                let Some(spec) = curr.clone().into_inner().next() else { return };
                let kind = spec.as_rule();
                let mut it = spec.into_inner();
                let step = |p: Option<Pair<Rule>>| p.map_or(1, |s| s.as_str().parse::<usize>().unwrap_or(usize::MAX));
                let (start, end, step) = match kind {
                    Rule::right_range => {
                        let Some(e) = it.next() else { return };
                        (0, abs_index(parse_index(e.as_str()), n), step(it.next()))
                    }
                    Rule::all_range => (0, n, step(it.next())),
                    Rule::left_range => {
                        let Some(s) = it.next() else { return };
                        (abs_index(parse_index(s.as_str()), n), n, step(it.next()))
                    }
                    Rule::full_range => {
                        let (Some(s), Some(e)) = (it.next(), it.next()) else { return };
                        (abs_index(parse_index(s.as_str()), n), abs_index(parse_index(e.as_str()), n), step(it.next()))
                    }
                    _ => return,
                };
                for i in (start..end).step_by(step) {
                    if let Some(e) = json.index(i) {
                        path.push(Step::Index(i));
                        self.walk(rest, e, path, out);
                        path.pop();
                    }
                }
            }
            Rule::filter => {
                if matches!(json.ty(), Ty::Array | Ty::Object) {
                    self.each(json, path, &mut |c, v, p| {
                        if c.filter(curr.clone().into_inner(), v) {
                            c.walk(rest, v, p, out);
                        }
                    });
                }
            }
            _ => {}
        }
    }

    /// Visit the elements or members of `json` with their steps pushed.
    fn each(&self, json: Node<'j>, path: &mut Vec<Step>, f: &mut dyn FnMut(&Self, Node<'j>, &mut Vec<Step>)) {
        if let Some(o) = json.obj() {
            for (k, v) in o.iter() {
                path.push(Step::Key(k.to_string()));
                f(self, Node::Ref(v), path);
                path.pop();
            }
        } else if let Some(a) = json.arr() {
            for (i, v) in a.iter().enumerate() {
                path.push(Step::Index(i));
                f(self, v, path);
                path.pop();
            }
        }
    }

    fn full_scan<'i>(&self, rest: &[Pair<'i, Rule>], json: Node<'j>, path: &mut Vec<Step>, out: &mut Out<'j>) {
        self.each(json, path, &mut |c, v, p| {
            c.walk(rest, v, p, out);
            c.full_scan(rest, v, p, out);
        });
    }

    fn subquery<'i>(&self, root: Option<Pair<'i, Rule>>, json: Node<'j>) -> T<'i, 'j> {
        let Some(root) = root else { return T::Value(json) };
        let steps: Vec<_> = root.into_inner().collect();
        let mut out = Out { found: Vec::new(), track: false };
        self.walk(&steps, json, &mut Vec::new(), &mut out);
        match out.found.len() {
            0 => T::Invalid,
            1 => T::Value(out.found.pop().expect("one result").node),
            _ => T::NodeList(out.found.into_iter().map(|f| f.node).collect()),
        }
    }

    fn term<'i>(&self, term: Pair<'i, Rule>, json: Node<'j>) -> T<'i, 'j> {
        match term.as_rule() {
            Rule::decimal => {
                let s = term.as_str();
                if let Ok(i) = s.parse::<i64>() {
                    T::Integer(i)
                } else if let Ok(f) = s.parse::<f64>() {
                    T::Float(f)
                } else {
                    T::Invalid
                }
            }
            Rule::boolean_true => T::Bool(true),
            Rule::boolean_false => T::Bool(false),
            Rule::null => T::Null,
            Rule::array_literal | Rule::object_literal => {
                let key = term.as_span().start();
                let lit = self.literals.borrow_mut().entry(key).or_insert_with(|| Rc::new(build_literal(term))).clone();
                T::Literal(lit)
            }
            Rule::function_call => {
                let mut inner = term.into_inner();
                let name = inner.next().map_or("", |p| p.as_str());
                let args = inner.map(|a| self.term(a, json)).collect();
                eval_function(name, args, &mut self.regex.borrow_mut())
            }
            Rule::string_value | Rule::string_value_escape_1 | Rule::string_value_escape_2 => T::Str(unescape(&term)),
            Rule::from_current => self.subquery(term.into_inner().next(), json),
            Rule::from_root => self.subquery(term.into_inner().next(), self.root),
            _ => T::Invalid,
        }
    }

    fn expr<'i>(&self, expr: Pair<'i, Rule>, json: Node<'j>) -> T<'i, 'j> {
        // arith_expr and arith_term alternate operands and operators.
        let factor = expr.as_rule() == Rule::arith_term;
        let mut inner = expr.into_inner();
        let Some(first) = inner.next() else { return T::Invalid };
        let eval = |p: Pair<'i, Rule>| if factor { self.factor(p, json) } else { self.expr(p, json) };
        let mut acc = eval(first);
        while let Some(oper) = inner.next() {
            let Some(rhs) = inner.next() else { return T::Invalid };
            let rhs = eval(rhs);
            acc = arith_binop(oper.as_rule(), &acc, &rhs);
        }
        acc
    }

    fn factor<'i>(&self, factor: Pair<'i, Rule>, json: Node<'j>) -> T<'i, 'j> {
        let mut inner = factor.into_inner();
        let Some(first) = inner.next() else { return T::Invalid };
        match first.as_rule() {
            Rule::neg | Rule::pos => match inner.next() {
                Some(operand) => arith_unary(first.as_rule(), self.operand(operand, json)),
                None => T::Invalid,
            },
            _ => self.operand(first, json),
        }
    }

    fn operand<'i>(&self, p: Pair<'i, Rule>, json: Node<'j>) -> T<'i, 'j> {
        match p.as_rule() {
            Rule::arith_expr => self.expr(p, json),
            Rule::method_chain => {
                let mut it = p.into_inner();
                let Some(recv) = it.next() else { return T::Invalid };
                let mut acc = self.term(recv, json);
                for m in it {
                    if m.as_rule() == Rule::get_keys_op {
                        acc = eval_function("keys", vec![acc], &mut self.regex.borrow_mut());
                        continue;
                    }
                    let mut mi = m.into_inner();
                    let name = mi.next().map_or("", |p| p.as_str());
                    let mut args = vec![acc];
                    args.extend(mi.map(|a| self.operand(a, json)));
                    acc = eval_function(name, args, &mut self.regex.borrow_mut());
                }
                acc
            }
            _ => self.term(p, json),
        }
    }

    fn single_filter(&self, p: Pair<Rule>, json: Node<'j>) -> bool {
        let mut it = p.into_inner();
        let Some(a) = it.next() else { return false };
        let a = self.expr(a, json);
        let Some(oper) = it.next() else {
            return match a {
                T::Bool(b) => b,
                T::Results(vs) => !vs.is_empty(),
                other => !matches!(other, T::Invalid),
            };
        };
        let Some(b) = it.next() else { return false };
        let b = self.expr(b, json);
        match oper.as_rule() {
            Rule::gt => a.ord(&b, Ordering::is_gt),
            Rule::ge => a.ord(&b, Ordering::is_ge),
            Rule::lt => a.ord(&b, Ordering::is_lt),
            Rule::le => a.ord(&b, Ordering::is_le),
            Rule::eq => a.eq(&b),
            Rule::ne => !a.eq(&b),
            Rule::re => a.re(&b, &mut self.regex.borrow_mut()),
            Rule::in_op => a.member_of(&b),
            Rule::nin_op => !a.member_of(&b),
            Rule::subsetof_op => a.set_relate(&b, true),
            Rule::anyof_op => a.set_relate(&b, false),
            Rule::noneof_op => !a.set_relate(&b, false),
            Rule::size_op => a.size_of(&b),
            Rule::empty_op => a.empty_check(&b),
            _ => false,
        }
    }

    fn filter_operand(&self, p: Pair<Rule>, json: Node<'j>) -> bool {
        match p.as_rule() {
            Rule::single_filter => self.single_filter(p, json),
            Rule::filter => self.filter(p.into_inner(), json),
            Rule::negation => p.into_inner().next().is_some_and(|i| !self.filter_operand(i, json)),
            _ => false,
        }
    }

    /// Operands joined by && and ||, && binding tighter; evaluation stops once decided.
    fn filter(&self, mut it: Pairs<Rule>, json: Node<'j>) -> bool {
        let Some(first) = it.next() else { return false };
        let mut result = self.filter_operand(first, json);
        while let Some(rel) = it.next() {
            match rel.as_rule() {
                Rule::and => {
                    let Some(next) = it.next() else { return false };
                    if result {
                        result = self.filter_operand(next, json);
                    }
                }
                Rule::or => {
                    if result {
                        break;
                    }
                    return self.filter(it, json);
                }
                _ => return false,
            }
        }
        result
    }
}

fn parse_index(s: &str) -> i64 {
    s.parse::<i64>().unwrap_or(if s.starts_with('-') { i64::MIN } else { i64::MAX })
}

fn abs_index(i: i64, n: usize) -> usize {
    if i >= 0 { (i as usize).min(n) } else { (i + n as i64).max(0) as usize }
}

fn build_literal(p: Pair<Rule>) -> J {
    match p.as_rule() {
        Rule::array_literal => J::Arr(Box::new(Arr::het_from(p.into_inner().map(build_literal).collect()))),
        Rule::object_literal => {
            let mut o = Obj::new();
            for m in p.into_inner() {
                let mut it = m.into_inner();
                let k = it.next().map(|k| unescape(&k).into_owned()).unwrap_or_default();
                let v = it.next().map_or(J::Null, build_literal);
                o.insert(&k, v);
            }
            J::Obj(Box::new(o))
        }
        Rule::decimal => {
            let s = p.as_str();
            if let Ok(i) = s.parse::<i64>() {
                J::Num(N::I(i))
            } else {
                s.parse::<f64>().ok().and_then(N::float).map_or(J::Null, J::Num)
            }
        }
        Rule::string_value | Rule::string_value_escape_1 | Rule::string_value_escape_2 => J::str(&unescape(&p)),
        Rule::boolean_true => J::Bool(true),
        Rule::boolean_false => J::Bool(false),
        _ => J::Null,
    }
}

/// The values a path selects, in order.
pub fn select<'j>(q: &Query, doc: &'j J) -> Vec<Node<'j>> {
    run(q, doc, false).into_iter().map(|f| f.node).collect()
}

/// The values a path selects with their locations.
pub fn select_paths<'j>(q: &Query, doc: &'j J) -> Vec<Found<'j>> {
    run(q, doc, true)
}

fn run<'j>(q: &Query, doc: &'j J, track: bool) -> Vec<Found<'j>> {
    let calc = Calc::new(Node::Ref(doc));
    let mut out = Out { found: Vec::new(), track };
    calc.walk(&q.root, Node::Ref(doc), &mut Vec::new(), &mut out);
    out.found
}

/// The values a projection computes (none for Nothing).
pub fn project(q: &Query, doc: &J) -> Vec<J> {
    let Some(expr) = q.projection.clone() else { return Vec::new() };
    let calc = Calc::new(Node::Ref(doc));
    outputs(calc.expr(expr, Node::Ref(doc)))
}

/// A path as given, and as evaluated: legacy paths (`.a.b`, `a`) are rooted at `$`.
#[derive(Clone, Debug)]
pub struct Path<'a> {
    original: &'a str,
    fixed: Option<String>,
}

impl<'a> Path<'a> {
    pub fn new(path: &'a str) -> Path<'a> {
        let fixed = (!is_jsonpath(path)).then(|| {
            if path == "." {
                "$".to_string()
            } else if path.starts_with('.') {
                format!("${path}")
            } else {
                format!("$.{path}")
            }
        });
        Path { original: path, fixed }
    }

    pub fn is_legacy(&self) -> bool {
        self.fixed.is_some()
    }

    pub fn get(&self) -> &str {
        self.fixed.as_deref().unwrap_or(self.original)
    }

    pub fn original(&self) -> &'a str {
        self.original
    }
}

fn is_jsonpath(path: &str) -> bool {
    let b = path.as_bytes();
    let skip = |mut i: usize| {
        while b.get(i) == Some(&b' ') {
            i += 1;
        }
        i
    };
    let rooted_or_paren = |i: usize| match b.get(i) {
        Some(b'$') => i + 1 >= b.len() || matches!(b.get(i + 1), Some(b'.' | b'[' | b'~')),
        Some(b'(') => true,
        _ => false,
    };
    match b.first() {
        Some(b'$') => rooted_or_paren(0),
        Some(b'(') => true,
        Some(b'-' | b'+') => rooted_or_paren(skip(1)),
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {
            let n = b.iter().take_while(|x| x.is_ascii_alphanumeric() || **x == b'_').count();
            b.get(skip(n)) == Some(&b'(')
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::de::parse;
    use crate::json::ser::compact;

    fn get(doc: &str, path: &str) -> String {
        let doc = parse(doc, None).unwrap();
        let q = compile(path).unwrap();
        if q.is_projection() {
            compact(&project(&q, &doc))
        } else {
            compact(&select(&q, &doc))
        }
    }

    #[test]
    fn selection() {
        let d = r#"{"a":[1,2,3,{"b":5}],"c":{"b":"x"},"b":true}"#;
        assert_eq!(get(d, "$.a[1]"), "[2]");
        assert_eq!(get(d, "$.a[-1:]"), r#"[{"b":5}]"#);
        assert_eq!(get(d, "$.a[0:3:2]"), "[1,3]");
        assert_eq!(get(d, "$..b"), r#"[true,5,"x"]"#);
        assert_eq!(get(d, "$.a[?@>1]"), "[2,3]");
        assert_eq!(get(d, "$.a[?(@.b==5 || @==1)]"), r#"[1,{"b":5}]"#);
        assert_eq!(get(d, "$.*"), r#"[[1,2,3,{"b":5}],{"b":"x"},true]"#);
        assert_eq!(get(d, r#"$["c"]['b']"#), r#"["x"]"#);
        assert_eq!(get(d, "$.a.length()"), "[4]");
        assert_eq!(get(d, "$.a[0] + $.a[1] * 2"), "[5]");
        assert_eq!(get(d, "$.c~"), r#"["b"]"#);
        assert_eq!(get(d, "$.a[2] / 0"), "[]");
    }

    #[test]
    fn syntax_errors() {
        assert_eq!(compile("$.").unwrap_err(), "Error occurred at position 2, Error at position 2: expected one of the following: <string>, function_name, '*'");
        assert_eq!(compile("$.a[1:2:0]").unwrap_err(), "Error occurred at position 8, Error at position 8: expected one of the following: pos_number");
        assert_eq!(compile("$.x y").unwrap_err(), "Error occurred at position 4, Error at position 4: expected one of the following: EOI, add, sub, mul, div, rem, postfix_method, get_keys_op, '..'");
        assert!(compile(&("$".to_string() + &"[".repeat(129))).unwrap_err().contains("nesting too deep"));
    }

    #[test]
    fn legacy_paths() {
        assert_eq!(Path::new(".").get(), "$");
        assert_eq!(Path::new(".a").get(), "$.a");
        assert_eq!(Path::new("a").get(), "$.a");
        assert_eq!(Path::new("$a").get(), "$.$a");
        assert!(!Path::new("$.a").is_legacy());
        assert!(!Path::new("length($.a)").is_legacy());
        assert!(Path::new("-$x").is_legacy());
    }
}
