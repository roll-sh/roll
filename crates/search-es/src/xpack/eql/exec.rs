//! EQL execution: verification against the indices' mappings, translation of conditions into
//! query DSL, and the event, sequence and sample algorithms.
//!
//! **Conditions** are split the way Elasticsearch's translator splits them: `and`, `or` and
//! `not` (and `!=`, `not in`) stay structural, every other node is a leaf. A leaf Elasticsearch
//! turns into a query (`==`, ranges, `:`, `like`, `regex`, `in`, null checks, boolean fields,
//! `startsWith`) becomes that query; the rest (functions, arithmetic) are scripts on
//! Elasticsearch and are evaluated here on the event's doc-value fields with the scripts'
//! semantics (three-valued inside a leaf, `null` is false at the boolean level). A condition
//! made only of query leaves runs as one query. Otherwise the search gets the tightest query
//! the query leaves allow, every query leaf is also attached as a named query, and each hit's
//! `matched_queries` plus its fields decide the condition exactly — so a leaf always has the
//! query semantics it has on Elasticsearch (`!=` matches events without the field, a term on a
//! keyword with a normalizer normalizes, ...).
//!
//! **Ordering** is `timestamp`, then the optional tiebreaker, then `_shard_doc` within a point
//! in time, like Elasticsearch's implicit tiebreaker; pages are read with `search_after`.
//!
//! **Sequences** run Elasticsearch's matcher (`SequenceMatcher`): stages are matched in order,
//! a stage's event extends the latest in-flight sequence of the previous stage for the same
//! join key whose last event is before it, dropping that sequence and the older ones;
//! `maxspan` and `until` reject candidates; completed sequences are ordered by their last
//! event. Each stage's events are read once as (ordinal, key, id) — sources are fetched only
//! for the sequences returned — and the number of events held is bounded.
//!
//! **Limits** follow Elasticsearch's plan: the request `size` becomes a head or tail limit,
//! `| head`/`| tail` pipes combine with it into one limit with an offset (`CombineLimits`).
//! Event queries read in `result_position` order and present ascending; sequences take the
//! limit's view of the ascending completed list.

use super::parser::{ArithOp, CmpOp, EventFilter, Expr, ExprKind, Lit, MatchKind, Pipe, Query, Statement, Term, line_col};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::{enc, internal};
use std::cmp::Ordering;
use std::collections::HashMap;

/// At most this many events are held for one sequence or sample query (Elasticsearch's
/// `eql_sequence` circuit breaker plays this role).
const MAX_HELD_EVENTS: usize = 2_000_000;
/// Largest page of one search.
const MAX_PAGE: usize = 10_000;

/// The request settings the execution needs.
#[derive(Clone, Debug)]
pub struct Settings {
    pub index: String,
    pub timestamp_field: String,
    pub tiebreaker_field: Option<String>,
    pub event_category_field: String,
    pub size: i32,
    pub fetch_size: usize,
    pub filter: Option<Value>,
    pub runtime_mappings: Option<Value>,
    pub fields: Option<Value>,
    pub tail: bool,
    pub max_samples_per_key: usize,
    pub ignore_unavailable: bool,
    pub expand_wildcards: String,
}

/// What a query returned.
pub enum Output {
    Events(Vec<Value>),
    Sequences(Vec<Value>),
}

// --- schema ----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FType {
    Keyword,
    Text,
    Long,
    Double,
    Bool,
    Date,
    DateNanos,
    Ip,
    Other,
}

#[derive(Clone, Debug)]
struct FieldInfo {
    ty: FType,
    /// The mapping type (for messages).
    type_name: String,
    /// The field queries and doc values use: the field itself, or a text field's keyword
    /// multi-field.
    exact: Option<String>,
}

struct Schema {
    fields: HashMap<String, FieldInfo>,
}

fn ftype(t: &str) -> FType {
    match t {
        "keyword" | "constant_keyword" | "wildcard" | "version" => FType::Keyword,
        "text" | "match_only_text" => FType::Text,
        "long" | "integer" | "short" | "byte" | "unsigned_long" => FType::Long,
        "double" | "float" | "half_float" | "scaled_float" => FType::Double,
        "boolean" => FType::Bool,
        "date" => FType::Date,
        "date_nanos" => FType::DateNanos,
        "ip" => FType::Ip,
        _ => FType::Other,
    }
}

/// Elasticsearch's data type names (`DataTypes`), as messages print them.
fn type_display(t: &str) -> &str {
    match t {
        "date" => "datetime",
        "match_only_text" => "text",
        other => other,
    }
}

async fn schema(cluster: &Cluster, s: &Settings) -> EsResult<Schema> {
    let uri = format!(
        "/{}/_field_caps?fields=*&ignore_unavailable={}&allow_no_indices=true&expand_wildcards={}",
        enc(&s.index),
        s.ignore_unavailable,
        enc(&s.expand_wildcards)
    );
    let unknown = || {
        let msg = format!("Unknown index [{}]", s.index);
        EsError::index_not_found_bare(&msg).caused_by(EsError::new(400, "verification_exception", format!("Found 1 problem\nline -1:-1: {msg}")))
    };
    let caps = internal::call(cluster, "GET", &uri, None).await.map_err(|e| if e.kind == "index_not_found_exception" { unknown() } else { e })?;
    if caps.get("indices").and_then(Value::as_array).is_none_or(|a| a.is_empty()) {
        return Err(unknown());
    }
    let mut fields = HashMap::new();
    if let Some(Value::Obj(m)) = caps.get("fields") {
        for (name, types) in m {
            let Some(t) = types.as_object().and_then(|t| t.keys().find(|k| !matches!(k.as_str(), "object" | "nested")).or_else(|| t.keys().next())) else { continue };
            let ty = ftype(t);
            fields.insert(name.clone(), FieldInfo { ty, type_name: t.clone(), exact: (ty != FType::Text).then(|| name.clone()) });
        }
        // A text field's exact counterpart is its keyword multi-field.
        let mut subs: Vec<(String, String)> = Vec::new();
        for (name, info) in &fields {
            if info.ty == FType::Text {
                let prefix = format!("{name}.");
                let mut kids: Vec<&String> =
                    fields.iter().filter(|(k, f)| k.starts_with(&prefix) && !k[prefix.len()..].contains('.') && f.ty == FType::Keyword).map(|(k, _)| k).collect();
                kids.sort();
                if let Some(k) = kids.first() {
                    subs.push((name.clone(), (*k).clone()));
                }
            }
        }
        for (name, sub) in subs {
            if let Some(f) = fields.get_mut(&name) {
                f.exact = Some(sub);
            }
        }
    }
    Ok(Schema { fields })
}

// --- verification ------------------------------------------------------------------------------

/// The static type of an expression.
#[derive(Clone, Debug, PartialEq)]
enum SType {
    Null,
    Bool,
    Integer,
    Long,
    Double,
    Keyword,
    Field(String),
}

impl SType {
    fn name(&self) -> String {
        match self {
            SType::Null => "null".into(),
            SType::Bool => "boolean".into(),
            SType::Integer => "integer".into(),
            SType::Long => "long".into(),
            SType::Double => "double".into(),
            SType::Keyword => "keyword".into(),
            SType::Field(t) => type_display(t).to_string(),
        }
    }

    fn is_string(&self) -> bool {
        match self {
            SType::Keyword | SType::Null => true,
            SType::Field(t) => matches!(ftype(t), FType::Keyword | FType::Text),
            _ => false,
        }
    }

    fn is_numeric(&self) -> bool {
        match self {
            SType::Integer | SType::Long | SType::Double | SType::Null => true,
            SType::Field(t) => matches!(ftype(t), FType::Long | FType::Double),
            _ => false,
        }
    }

    fn is_bool(&self) -> bool {
        match self {
            SType::Bool => true,
            SType::Field(t) => ftype(t) == FType::Bool,
            _ => false,
        }
    }
}

struct Verifier<'a> {
    src: &'a str,
    schema: &'a Schema,
    problems: Vec<(usize, String)>,
}

const ORDINALS: [&str; 4] = ["first", "second", "third", "fourth"];

/// A function's arity.
struct FnSig {
    name: &'static str,
    min: usize,
    max: usize,
    insensitive: bool,
}

const FUNCTIONS: &[FnSig] = &[
    FnSig { name: "between", min: 3, max: 4, insensitive: true },
    FnSig { name: "cidrmatch", min: 2, max: usize::MAX, insensitive: false },
    FnSig { name: "concat", min: 0, max: usize::MAX, insensitive: false },
    FnSig { name: "endswith", min: 2, max: 2, insensitive: true },
    FnSig { name: "indexof", min: 2, max: 3, insensitive: true },
    FnSig { name: "length", min: 1, max: 1, insensitive: false },
    FnSig { name: "startswith", min: 2, max: 2, insensitive: true },
    FnSig { name: "string", min: 1, max: 1, insensitive: false },
    FnSig { name: "stringcontains", min: 2, max: 2, insensitive: true },
    FnSig { name: "substring", min: 2, max: 3, insensitive: false },
    FnSig { name: "add", min: 2, max: 2, insensitive: false },
    FnSig { name: "divide", min: 2, max: 2, insensitive: false },
    FnSig { name: "modulo", min: 2, max: 2, insensitive: false },
    FnSig { name: "multiply", min: 2, max: 2, insensitive: false },
    FnSig { name: "number", min: 1, max: 2, insensitive: false },
    FnSig { name: "subtract", min: 2, max: 2, insensitive: false },
];

fn arity_message(sig: &FnSig) -> &'static str {
    match (sig.min, sig.max) {
        (1, 1) => "expects exactly one argument",
        (2, 2) => "expects exactly two arguments",
        (1, 2) => "expects one or two arguments",
        (2, 3) => "expects two or three arguments",
        (3, 4) => "expects three or four arguments",
        (2, _) => "expects at least two arguments",
        _ => "expects at least one argument",
    }
}

impl Verifier<'_> {
    fn text(&self, e: &Expr) -> String {
        self.src[e.start..e.end].to_string()
    }

    fn problem(&mut self, at: usize, msg: String) {
        self.problems.push((at, msg));
    }

    /// Resolves fields (unknown optional fields become `null`), checks types, and returns the
    /// rewritten expression with its type.
    fn resolve(&mut self, e: &Expr) -> EsResult<(Expr, Option<SType>)> {
        let mut out = e.clone();
        let ty = match &e.kind {
            ExprKind::Lit(l) => Some(match l {
                Lit::Null => SType::Null,
                Lit::Bool(_) => SType::Bool,
                Lit::Int(i) => {
                    if i32::try_from(*i).is_ok() {
                        SType::Integer
                    } else {
                        SType::Long
                    }
                }
                Lit::Double(_) => SType::Double,
                Lit::Str(_) => SType::Keyword,
            }),
            ExprKind::Field { name, optional } => match self.schema.fields.get(name) {
                Some(f) => Some(SType::Field(f.type_name.clone())),
                None if *optional => {
                    out.kind = ExprKind::Lit(Lit::Null);
                    Some(SType::Null)
                }
                None => {
                    self.problem(e.start, format!("Unknown column [{name}]"));
                    None
                }
            },
            ExprKind::Not(x) => {
                let (x, _) = self.resolve(x)?;
                out.kind = ExprKind::Not(Box::new(x));
                Some(SType::Bool)
            }
            ExprKind::And(a, b) | ExprKind::Or(a, b) => {
                let (a2, _) = self.resolve(a)?;
                let (b2, _) = self.resolve(b)?;
                out.kind = if matches!(e.kind, ExprKind::And(..)) { ExprKind::And(Box::new(a2), Box::new(b2)) } else { ExprKind::Or(Box::new(a2), Box::new(b2)) };
                Some(SType::Bool)
            }
            ExprKind::Cmp(op, a, b) => {
                let (a2, _) = self.resolve(a)?;
                let (b2, _) = self.resolve(b)?;
                // Field-to-field comparisons are refused before verification on Elasticsearch.
                if let (ExprKind::Field { .. }, ExprKind::Field { name, .. }) = (&a2.kind, &b2.kind) {
                    let (line, col) = line_col(self.src, b.start);
                    let sym = match op {
                        CmpOp::Eq => "==",
                        CmpOp::Neq => "!=",
                        CmpOp::Lt => "<",
                        CmpOp::Lte => "<=",
                        CmpOp::Gt => ">",
                        CmpOp::Gte => ">=",
                    };
                    return Err(EsError::new(
                        400,
                        "invalid_argument_exception",
                        format!("Line {line}:{col}: Comparisons against fields are not (currently) supported; offender [{name}] in [{sym}]"),
                    ));
                }
                self.check_exact(e, &a2, "first");
                self.check_exact(e, &b2, "second");
                out.kind = ExprKind::Cmp(*op, Box::new(a2), Box::new(b2));
                Some(SType::Bool)
            }
            ExprKind::Arith(op, a, b) => {
                let (a2, ta) = self.resolve(a)?;
                let (b2, tb) = self.resolve(b)?;
                for (i, (x, t)) in [(a, &ta), (b, &tb)].into_iter().enumerate() {
                    if let Some(t) = t
                        && !t.is_numeric()
                    {
                        let msg = format!("{} argument of [{}] must be [numeric], found value [{}] type [{}]", ORDINALS[i], self.text(e), self.text(x), t.name());
                        self.problem(e.start, msg);
                    }
                }
                let ty = match (&ta, &tb) {
                    (Some(a), Some(b)) if is_floating(a) || is_floating(b) => SType::Double,
                    (Some(SType::Integer), Some(SType::Integer)) => SType::Integer,
                    _ => SType::Long,
                };
                out.kind = ExprKind::Arith(*op, Box::new(a2), Box::new(b2));
                Some(ty)
            }
            ExprKind::Neg(x) => {
                let (x2, t) = self.resolve(x)?;
                if let Some(t) = &t
                    && !t.is_numeric()
                {
                    let msg = format!("argument of [{}] must be [numeric], found value [{}] type [{}]", self.text(e), self.text(x), t.name());
                    self.problem(e.start, msg);
                }
                out.kind = ExprKind::Neg(Box::new(x2));
                t
            }
            ExprKind::In { expr, list, insensitive, negated } => {
                let (x, _) = self.resolve(expr)?;
                self.check_exact(e, &x, "first");
                let mut l2 = Vec::with_capacity(list.len());
                for item in list {
                    l2.push(self.resolve(item)?.0);
                }
                out.kind = ExprKind::In { expr: Box::new(x), list: l2, insensitive: *insensitive, negated: *negated };
                Some(SType::Bool)
            }
            ExprKind::Match { kind, expr, patterns } => {
                let (x, t) = self.resolve(expr)?;
                self.check_exact(e, &x, "first");
                if let Some(t) = &t
                    && !t.is_string()
                {
                    let msg = format!("first argument of [{}] must be [string], found value [{}] type [{}]", self.text(e), self.text(expr), t.name());
                    self.problem(e.start, msg);
                }
                out.kind = ExprKind::Match { kind: *kind, expr: Box::new(x), patterns: patterns.clone() };
                Some(SType::Bool)
            }
            ExprKind::Call { name, insensitive, args } => {
                let lower = name.to_ascii_lowercase();
                let Some(sig) = FUNCTIONS.iter().find(|f| f.name == lower) else {
                    self.problem(e.start, format!("Unknown function [{name}]"));
                    return Ok((out, None));
                };
                if *insensitive && !sig.insensitive {
                    self.problem(e.start, format!("Function [{name}] does not support case-insensitive matching"));
                }
                if args.len() < sig.min || args.len() > sig.max {
                    return Err(super::parser::error_at(self.src, e.start + 1, format!("error building [{lower}]: {}", arity_message(sig))));
                }
                let mut resolved = Vec::with_capacity(args.len());
                let mut types = Vec::with_capacity(args.len());
                for a in args {
                    let (a2, t) = self.resolve(a)?;
                    resolved.push(a2);
                    types.push(t);
                }
                self.check_function(e, &lower, args, &types);
                let ty = match lower.as_str() {
                    "endswith" | "startswith" | "stringcontains" | "cidrmatch" => SType::Bool,
                    "length" | "indexof" => SType::Integer,
                    "number" => SType::Long,
                    "add" | "divide" | "modulo" | "multiply" | "subtract" => {
                        if types.iter().flatten().any(is_floating) {
                            SType::Double
                        } else {
                            SType::Long
                        }
                    }
                    _ => SType::Keyword,
                };
                out.kind = ExprKind::Call { name: lower, insensitive: *insensitive, args: resolved };
                Some(ty)
            }
        };
        Ok((out, ty))
    }

    fn check_function(&mut self, e: &Expr, name: &str, args: &[Expr], types: &[Option<SType>]) {
        let want = |i: usize| -> Option<&'static str> {
            Some(match (name, i) {
                ("between", 0..=2) | ("endswith" | "startswith" | "stringcontains", 0..=1) | ("indexof", 0..=1) | ("length" | "substring" | "number", 0) => "string",
                ("between", 3) => "boolean",
                ("indexof", 2) | ("substring", 1..=2) | ("number", 1) => "integer",
                ("add" | "divide" | "modulo" | "multiply" | "subtract", _) => "numeric",
                ("cidrmatch", 0) => "ip",
                ("cidrmatch", _) => "string",
                _ => return None,
            })
        };
        for (i, (a, t)) in args.iter().zip(types).enumerate() {
            let (Some(kind), Some(t)) = (want(i), t) else { continue };
            let ok = match kind {
                "string" => t.is_string(),
                "boolean" => t.is_bool() || *t == SType::Null,
                "integer" => matches!(t, SType::Integer | SType::Null) || matches!(t, SType::Field(f) if ftype(f) == FType::Long),
                "numeric" => t.is_numeric(),
                "ip" => matches!(t, SType::Null) || matches!(t, SType::Field(f) if ftype(f) == FType::Ip),
                _ => true,
            };
            if !ok {
                let ord = ORDINALS.get(i).copied().unwrap_or("fourth");
                let msg = format!("{ord} argument of [{}] must be [{kind}], found value [{}] type [{}]", self.text(e), self.text(a), t.name());
                self.problem(e.start, msg);
            }
        }
    }

    /// Text fields without a keyword multi-field can't be compared exactly.
    fn check_exact(&mut self, whole: &Expr, operand: &Expr, ordinal: &str) {
        if let ExprKind::Field { name, .. } = &operand.kind
            && let Some(f) = self.schema.fields.get(name)
            && f.ty == FType::Text
            && f.exact.is_none()
        {
            let msg = format!(
                "[{}] cannot operate on {ordinal} argument field of data type [text]: No keyword/multi-field defined exact matches for [{name}]; define one or use MATCH/QUERY instead",
                self.text(whole)
            );
            self.problem(whole.start, msg);
        }
    }

    fn finish(self) -> EsResult<()> {
        if self.problems.is_empty() {
            return Ok(());
        }
        let n = self.problems.len();
        let mut msg = format!("Found {n} problem{}", if n == 1 { "" } else { "s" });
        for (at, p) in &self.problems {
            if *at == usize::MAX {
                msg.push_str(&format!("\nline -1:-1: {p}"));
            } else {
                let (l, c) = line_col(self.src, *at);
                msg.push_str(&format!("\nline {l}:{c}: {p}"));
            }
        }
        Err(EsError::new(400, "verification_exception", msg))
    }
}

fn is_floating(t: &SType) -> bool {
    match t {
        SType::Double => true,
        SType::Field(f) => ftype(f) == FType::Double,
        _ => false,
    }
}

// --- condition plans ---------------------------------------------------------------------------

/// A condition split into query leaves and evaluated leaves.
#[derive(Clone, Debug, PartialEq)]
enum Node {
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
    /// A query leaf (index into [`Cond::leaves`]).
    Dsl(usize),
    /// A leaf evaluated on the event's fields.
    Mem(Expr),
    Const(bool),
}

/// A planned stage condition.
#[derive(Clone, Debug, PartialEq)]
struct Cond {
    root: Node,
    leaves: Vec<Value>,
    /// Fields evaluated leaves read (exact names).
    mem_fields: Vec<String>,
}

fn contains_mem(n: &Node) -> bool {
    match n {
        Node::And(a, b) | Node::Or(a, b) => contains_mem(a) || contains_mem(b),
        Node::Not(x) => contains_mem(x),
        Node::Mem(_) => true,
        Node::Dsl(_) | Node::Const(_) => false,
    }
}

impl Cond {
    fn has_mem(&self) -> bool {
        contains_mem(&self.root)
    }

    /// The exact query (no evaluated leaves below `n`).
    fn exact_query(&self, n: &Node) -> Value {
        match n {
            Node::And(a, b) => obj! { "bool" => obj! { "filter" => crate::arr![self.exact_query(a), self.exact_query(b)] } },
            Node::Or(a, b) => obj! { "bool" => obj! { "should" => crate::arr![self.exact_query(a), self.exact_query(b)], "minimum_should_match" => 1 } },
            Node::Not(x) => obj! { "bool" => obj! { "must_not" => crate::arr![self.exact_query(x)] } },
            Node::Dsl(i) => self.leaves[*i].clone(),
            Node::Const(true) | Node::Mem(_) => obj! { "match_all" => obj! {} },
            Node::Const(false) => obj! { "match_none" => obj! {} },
        }
    }

    /// A query matching at least every event the condition matches (`None`: all of them).
    fn superset(&self, n: &Node) -> Option<Value> {
        match n {
            Node::And(a, b) => match (self.superset(a), self.superset(b)) {
                (Some(x), Some(y)) => Some(obj! { "bool" => obj! { "filter" => crate::arr![x, y] } }),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            },
            Node::Or(a, b) => {
                let (x, y) = (self.superset(a)?, self.superset(b)?);
                Some(obj! { "bool" => obj! { "should" => crate::arr![x, y], "minimum_should_match" => 1 } })
            }
            Node::Not(x) => {
                if contains_mem(x) {
                    None
                } else {
                    Some(obj! { "bool" => obj! { "must_not" => crate::arr![self.exact_query(x)] } })
                }
            }
            Node::Dsl(i) => Some(self.leaves[*i].clone()),
            Node::Const(true) | Node::Mem(_) => None,
            Node::Const(false) => Some(obj! { "match_none" => obj! {} }),
        }
    }
}

struct Planner<'a> {
    schema: &'a Schema,
    leaves: Vec<Value>,
    mem_fields: Vec<String>,
}

fn lit_json(l: &Lit) -> Value {
    match l {
        Lit::Null => Value::Null,
        Lit::Bool(b) => Value::Bool(*b),
        Lit::Int(i) => Value::from(*i),
        Lit::Double(d) => Value::from(*d),
        Lit::Str(s) => Value::from(s.clone()),
    }
}

/// An EQL wildcard pattern as a Lucene wildcard (only `*` and `?` are special; a backslash is
/// literal).
fn lucene_wildcard(p: &str) -> String {
    p.replace('\\', "\\\\")
}

fn has_wildcard(p: &str) -> bool {
    p.contains('*') || p.contains('?')
}

impl Planner<'_> {
    fn exact(&self, name: &str) -> String {
        self.schema.fields.get(name).and_then(|f| f.exact.clone()).unwrap_or_else(|| name.to_string())
    }

    fn leaf(&mut self, q: Value) -> Node {
        self.leaves.push(q);
        Node::Dsl(self.leaves.len() - 1)
    }

    fn plan(&mut self, e: &Expr) -> Node {
        match &e.kind {
            ExprKind::And(a, b) => Node::And(Box::new(self.plan(a)), Box::new(self.plan(b))),
            ExprKind::Or(a, b) => Node::Or(Box::new(self.plan(a)), Box::new(self.plan(b))),
            ExprKind::Not(x) => Node::Not(Box::new(self.plan(x))),
            ExprKind::Cmp(CmpOp::Neq, a, b) => {
                let eq = Expr { kind: ExprKind::Cmp(CmpOp::Eq, a.clone(), b.clone()), start: e.start, end: e.end };
                Node::Not(Box::new(self.plan(&eq)))
            }
            ExprKind::In { expr, list, insensitive, negated: true } => {
                let pos = Expr { kind: ExprKind::In { expr: expr.clone(), list: list.clone(), insensitive: *insensitive, negated: false }, start: e.start, end: e.end };
                Node::Not(Box::new(self.plan(&pos)))
            }
            _ => match self.dsl(e) {
                Some(Ok(q)) => self.leaf(q),
                Some(Err(c)) => Node::Const(c),
                None => {
                    collect_fields(e, &mut self.mem_fields, self.schema);
                    Node::Mem(e.clone())
                }
            },
        }
    }

    /// The query of a leaf (`Err(b)`: a constant; `None`: evaluated in memory).
    fn dsl(&self, e: &Expr) -> Option<Result<Value, bool>> {
        let field = |x: &Expr| match &x.kind {
            ExprKind::Field { name, .. } => Some(name.clone()),
            _ => None,
        };
        let lit = |x: &Expr| match &x.kind {
            ExprKind::Lit(l) => Some(l.clone()),
            _ => None,
        };
        match &e.kind {
            ExprKind::Lit(Lit::Bool(b)) => Some(Err(*b)),
            ExprKind::Lit(_) => Some(Err(false)),
            ExprKind::Field { name, .. } => Some(Ok(obj! { "term" => obj! { self.exact(name) => true } })),
            ExprKind::Cmp(op, a, b) => {
                let (f, l, flipped) = match (field(a), lit(b), field(b), lit(a)) {
                    (Some(f), Some(l), _, _) => (f, l, false),
                    (_, _, Some(f), Some(l)) => (f, l, true),
                    _ => return None,
                };
                let f = self.exact(&f);
                if l == Lit::Null {
                    let exists = obj! { "exists" => obj! { "field" => f } };
                    return Some(match op {
                        CmpOp::Eq => Ok(obj! { "bool" => obj! { "must_not" => crate::arr![exists] } }),
                        CmpOp::Neq => Ok(exists),
                        _ => Err(false),
                    });
                }
                let v = lit_json(&l);
                let rop = match (op, flipped) {
                    (CmpOp::Eq, _) => return Some(Ok(obj! { "term" => obj! { f => v } })),
                    (CmpOp::Neq, _) => return None,
                    (CmpOp::Lt, false) | (CmpOp::Gt, true) => "lt",
                    (CmpOp::Lte, false) | (CmpOp::Gte, true) => "lte",
                    (CmpOp::Gt, false) | (CmpOp::Lt, true) => "gt",
                    (CmpOp::Gte, false) | (CmpOp::Lte, true) => "gte",
                };
                Some(Ok(obj! { "range" => obj! { f => obj! { rop => v } } }))
            }
            ExprKind::In { expr, list, insensitive, negated: false } => {
                let f = self.exact(&field(expr)?);
                let mut vals = Vec::new();
                for item in list {
                    match lit(item)? {
                        Lit::Null => {}
                        l => vals.push(lit_json(&l)),
                    }
                }
                if *insensitive {
                    let should: Vec<Value> = vals.into_iter().map(|v| obj! { "term" => obj! { f.clone() => obj! { "value" => v, "case_insensitive" => true } } }).collect();
                    return Some(Ok(obj! { "bool" => obj! { "should" => Value::Arr(should), "minimum_should_match" => 1 } }));
                }
                Some(Ok(obj! { "terms" => obj! { f => Value::Arr(vals) } }))
            }
            ExprKind::Match { kind, expr, patterns } => {
                let f = self.exact(&field(expr)?);
                let mut should = Vec::new();
                for p in patterns {
                    let Lit::Str(s) = lit(p)? else { return None };
                    should.push(match kind {
                        MatchKind::Seq if has_wildcard(&s) => obj! { "wildcard" => obj! { f.clone() => obj! { "value" => lucene_wildcard(&s), "case_insensitive" => true } } },
                        MatchKind::Seq => obj! { "term" => obj! { f.clone() => obj! { "value" => s, "case_insensitive" => true } } },
                        MatchKind::Like { insensitive } => {
                            if has_wildcard(&s) {
                                obj! { "wildcard" => obj! { f.clone() => obj! { "value" => lucene_wildcard(&s), "case_insensitive" => *insensitive } } }
                            } else {
                                obj! { "term" => obj! { f.clone() => obj! { "value" => s, "case_insensitive" => *insensitive } } }
                            }
                        }
                        MatchKind::Regex { insensitive } => obj! { "regexp" => obj! { f.clone() => obj! { "value" => s, "case_insensitive" => *insensitive } } },
                    });
                }
                if should.len() == 1 {
                    return should.pop().map(Ok);
                }
                Some(Ok(obj! { "bool" => obj! { "should" => Value::Arr(should), "minimum_should_match" => 1 } }))
            }
            ExprKind::Call { name, insensitive, args } if name == "startswith" && args.len() == 2 => {
                let f = self.exact(&field(&args[0])?);
                match lit(&args[1])? {
                    Lit::Str(s) => Some(Ok(obj! { "prefix" => obj! { f => obj! { "value" => s, "case_insensitive" => *insensitive } } })),
                    Lit::Null => Some(Err(false)),
                    _ => None,
                }
            }
            _ => {
                // Constant expressions fold.
                if !has_field(e) {
                    let v = eval(e, &Doc::default()).ok()?;
                    return Some(Err(matches!(v, V::Bool(true))));
                }
                None
            }
        }
    }
}

fn has_field(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Field { .. } => true,
        ExprKind::Lit(_) => false,
        ExprKind::Not(x) | ExprKind::Neg(x) => has_field(x),
        ExprKind::And(a, b) | ExprKind::Or(a, b) | ExprKind::Cmp(_, a, b) | ExprKind::Arith(_, a, b) => has_field(a) || has_field(b),
        ExprKind::In { expr, list, .. } => has_field(expr) || list.iter().any(has_field),
        ExprKind::Match { expr, .. } => has_field(expr),
        ExprKind::Call { args, .. } => args.iter().any(has_field),
    }
}

fn collect_fields(e: &Expr, out: &mut Vec<String>, schema: &Schema) {
    match &e.kind {
        ExprKind::Field { name, .. } => {
            let exact = schema.fields.get(name).and_then(|f| f.exact.clone()).unwrap_or_else(|| name.clone());
            if !out.contains(&exact) {
                out.push(exact);
            }
        }
        ExprKind::Lit(_) => {}
        ExprKind::Not(x) | ExprKind::Neg(x) => collect_fields(x, out, schema),
        ExprKind::And(a, b) | ExprKind::Or(a, b) | ExprKind::Cmp(_, a, b) | ExprKind::Arith(_, a, b) => {
            collect_fields(a, out, schema);
            collect_fields(b, out, schema);
        }
        ExprKind::In { expr, list, .. } => {
            collect_fields(expr, out, schema);
            list.iter().for_each(|x| collect_fields(x, out, schema));
        }
        ExprKind::Match { expr, .. } => collect_fields(expr, out, schema),
        ExprKind::Call { args, .. } => args.iter().for_each(|x| collect_fields(x, out, schema)),
    }
}

// --- evaluation ----------------------------------------------------------------------------------

/// A runtime value of an evaluated leaf.
#[derive(Clone, Debug, PartialEq)]
pub enum V {
    Null,
    Bool(bool),
    Long(i64),
    Double(f64),
    Str(String),
}

/// An event's field values (doc-value order), by exact field name, plus the exact name of
/// each field the query names.
#[derive(Default)]
struct Doc {
    values: HashMap<String, Vec<V>>,
    exact: HashMap<String, String>,
}

impl Doc {
    fn first(&self, name: &str) -> V {
        let key = self.exact.get(name).map_or(name, String::as_str);
        self.values.get(key).and_then(|v| v.first().cloned()).unwrap_or(V::Null)
    }
}

fn json_to_v(v: &Value) -> V {
    match v {
        Value::Null => V::Null,
        Value::Bool(b) => V::Bool(*b),
        Value::Num(n) => match n.as_i64() {
            Some(i) if n.is_integer() => V::Long(i),
            _ => V::Double(n.as_f64()),
        },
        Value::Str(s) => V::Str(s.clone()),
        other => V::Str(other.to_json()),
    }
}

fn v_cmp(a: &V, b: &V) -> Option<Ordering> {
    match (a, b) {
        (V::Long(x), V::Long(y)) => Some(x.cmp(y)),
        (V::Long(x), V::Double(y)) => (*x as f64).partial_cmp(y),
        (V::Double(x), V::Long(y)) => x.partial_cmp(&(*y as f64)),
        (V::Double(x), V::Double(y)) => x.partial_cmp(y),
        (V::Str(x), V::Str(y)) => Some(java_cmp(x, y)),
        (V::Bool(x), V::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Java's `String.compareTo` (UTF-16 code units).
fn java_cmp(a: &str, b: &str) -> Ordering {
    if a.is_ascii() && b.is_ascii() {
        return a.cmp(b);
    }
    a.encode_utf16().cmp(b.encode_utf16())
}

fn v_to_string(v: &V) -> Option<String> {
    match v {
        V::Null => None,
        V::Bool(b) => Some(b.to_string()),
        V::Long(i) => Some(i.to_string()),
        V::Double(d) => Some(java_double(*d)),
        V::Str(s) => Some(s.clone()),
    }
}

/// Java's `Double.toString` for the common cases (integral values keep `.0`).
fn java_double(d: f64) -> String {
    if d.is_finite() && d == d.trunc() && d.abs() < 1e7 {
        format!("{d:.1}")
    } else {
        format!("{d}")
    }
}

/// EQL wildcard matching (`*`, `?`) over chars.
fn wildcard_match(pattern: &str, s: &str, insensitive: bool) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = if insensitive {
        (pattern.to_lowercase().chars().collect(), s.to_lowercase().chars().collect())
    } else {
        (pattern.chars().collect(), s.chars().collect())
    };
    let (mut pi, mut si, mut star, mut mark) = (0usize, 0usize, usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn java_index_of(hay: &str, needle: &str, from: i64) -> Option<i64> {
    let h = utf16(hay);
    let n = utf16(needle);
    let start = from.max(0) as usize;
    if n.is_empty() {
        return Some(start.min(h.len()) as i64);
    }
    if start >= h.len() || n.len() > h.len() {
        return None;
    }
    (start..=h.len() - n.len()).find(|&i| h[i..i + n.len()] == n[..]).map(|i| i as i64)
}

fn eval_err(msg: String) -> EsError {
    EsError::new(400, "eql_illegal_argument_exception", msg)
}

fn as_num(v: &V) -> Option<f64> {
    match v {
        V::Long(i) => Some(*i as f64),
        V::Double(d) => Some(*d),
        _ => None,
    }
}

fn arith(op: ArithOp, a: V, b: V) -> EsResult<V> {
    if a == V::Null || b == V::Null {
        return Ok(V::Null);
    }
    if let (V::Long(x), V::Long(y)) = (&a, &b) {
        let (x, y) = (*x, *y);
        if matches!(op, ArithOp::Div | ArithOp::Mod) && y == 0 {
            return Err(EsError::java(500, "arithmetic_exception", "/ by zero"));
        }
        let r = match op {
            ArithOp::Add => x.checked_add(y),
            ArithOp::Sub => x.checked_sub(y),
            ArithOp::Mul => x.checked_mul(y),
            ArithOp::Div => x.checked_div(y),
            ArithOp::Mod => x.checked_rem(y),
        };
        return r.map(V::Long).ok_or_else(|| EsError::java(500, "arithmetic_exception", "long overflow"));
    }
    let (Some(x), Some(y)) = (as_num(&a), as_num(&b)) else { return Ok(V::Null) };
    Ok(V::Double(match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => x / y,
        ArithOp::Mod => x % y,
    }))
}

fn eval(e: &Expr, doc: &Doc) -> EsResult<V> {
    Ok(match &e.kind {
        ExprKind::Lit(l) => match l {
            Lit::Null => V::Null,
            Lit::Bool(b) => V::Bool(*b),
            Lit::Int(i) => V::Long(*i),
            Lit::Double(d) => V::Double(*d),
            Lit::Str(s) => V::Str(s.clone()),
        },
        ExprKind::Field { name, .. } => doc.first(name),
        ExprKind::Not(x) => match eval(x, doc)? {
            V::Bool(b) => V::Bool(!b),
            _ => V::Null,
        },
        ExprKind::And(a, b) => match (eval(a, doc)?, eval(b, doc)?) {
            (V::Bool(false), _) | (_, V::Bool(false)) => V::Bool(false),
            (V::Bool(true), V::Bool(true)) => V::Bool(true),
            _ => V::Null,
        },
        ExprKind::Or(a, b) => match (eval(a, doc)?, eval(b, doc)?) {
            (V::Bool(true), _) | (_, V::Bool(true)) => V::Bool(true),
            (V::Bool(false), V::Bool(false)) => V::Bool(false),
            _ => V::Null,
        },
        ExprKind::Cmp(op, a, b) => {
            // `x == null` and `x != null` are null checks.
            let null_lit = |x: &Expr| matches!(x.kind, ExprKind::Lit(Lit::Null));
            if matches!(op, CmpOp::Eq | CmpOp::Neq) && (null_lit(a) || null_lit(b)) {
                let other = if null_lit(a) { b } else { a };
                let is_null = eval(other, doc)? == V::Null;
                return Ok(V::Bool(if *op == CmpOp::Eq { is_null } else { !is_null }));
            }
            let (x, y) = (eval(a, doc)?, eval(b, doc)?);
            if x == V::Null || y == V::Null {
                return Ok(V::Null);
            }
            let Some(o) = v_cmp(&x, &y) else { return Ok(V::Bool(*op == CmpOp::Neq)) };
            V::Bool(match op {
                CmpOp::Eq => o == Ordering::Equal,
                CmpOp::Neq => o != Ordering::Equal,
                CmpOp::Lt => o == Ordering::Less,
                CmpOp::Lte => o != Ordering::Greater,
                CmpOp::Gt => o == Ordering::Greater,
                CmpOp::Gte => o != Ordering::Less,
            })
        }
        ExprKind::Arith(op, a, b) => arith(*op, eval(a, doc)?, eval(b, doc)?)?,
        ExprKind::Neg(x) => match eval(x, doc)? {
            V::Long(i) => V::Long(i.checked_neg().ok_or_else(|| EsError::java(500, "arithmetic_exception", "long overflow"))?),
            V::Double(d) => V::Double(-d),
            _ => V::Null,
        },
        ExprKind::In { expr, list, insensitive, negated } => {
            let x = eval(expr, doc)?;
            if x == V::Null {
                return Ok(V::Null);
            }
            let mut saw_null = false;
            let mut found = false;
            for item in list {
                let y = eval(item, doc)?;
                if y == V::Null {
                    saw_null = true;
                    continue;
                }
                let eq = match (&x, &y, insensitive) {
                    (V::Str(a), V::Str(b), true) => a.to_lowercase() == b.to_lowercase(),
                    _ => v_cmp(&x, &y) == Some(Ordering::Equal),
                };
                if eq {
                    found = true;
                    break;
                }
            }
            let r = if found {
                V::Bool(true)
            } else if saw_null {
                V::Null
            } else {
                V::Bool(false)
            };
            match (r, negated) {
                (V::Bool(b), true) => V::Bool(!b),
                (r, _) => r,
            }
        }
        ExprKind::Match { kind, expr, patterns } => {
            let V::Str(s) = eval(expr, doc)? else { return Ok(V::Null) };
            let mut any = false;
            for p in patterns {
                let ExprKind::Lit(Lit::Str(p)) = &p.kind else { continue };
                let m = match kind {
                    MatchKind::Seq => {
                        if has_wildcard(p) {
                            wildcard_match(p, &s, true)
                        } else {
                            s.to_lowercase() == p.to_lowercase()
                        }
                    }
                    MatchKind::Like { insensitive } => wildcard_match(p, &s, *insensitive),
                    MatchKind::Regex { insensitive } => {
                        let re = regex::RegexBuilder::new(&format!("^(?:{p})$")).case_insensitive(*insensitive).build().map_err(|e| eval_err(e.to_string()))?;
                        re.is_match(&s)
                    }
                };
                if m {
                    any = true;
                    break;
                }
            }
            V::Bool(any)
        }
        ExprKind::Call { name, insensitive, args } => {
            let mut a = Vec::with_capacity(args.len());
            for x in args {
                a.push(eval(x, doc)?);
            }
            call(name, *insensitive, a)?
        }
    })
}

fn str_arg(v: &V) -> EsResult<Option<String>> {
    match v {
        V::Null => Ok(None),
        V::Str(s) => Ok(Some(s.clone())),
        other => Err(eval_err(format!("A string/char is required; received [{}]", v_to_string(other).unwrap_or_default()))),
    }
}

fn int_arg(v: Option<&V>) -> EsResult<Option<i64>> {
    match v {
        None | Some(V::Null) => Ok(None),
        // Java's `Number.intValue()`.
        Some(V::Long(i)) => Ok(Some(*i as i32 as i64)),
        Some(V::Double(d)) => Ok(Some(*d as i32 as i64)),
        Some(other) => Err(eval_err(format!("A number is required; received [{}]", v_to_string(other).unwrap_or_default()))),
    }
}

fn call(name: &str, ci: bool, a: Vec<V>) -> EsResult<V> {
    let lower = |s: &str| if ci { s.to_lowercase() } else { s.to_string() };
    Ok(match name {
        "between" => {
            let Some(s) = str_arg(&a[0])? else { return Ok(V::Null) };
            let (Some(left), Some(right)) = (str_arg(&a[1])?, str_arg(&a[2])?) else { return Ok(V::Null) };
            let greedy = matches!(a.get(3), Some(V::Bool(true)));
            if s.is_empty() || left.is_empty() || right.is_empty() {
                return Ok(V::Str(s));
            }
            let m = lower(&s);
            let (l, r) = (lower(&left), lower(&right));
            let su = utf16(&s);
            let Some(idx) = java_index_of(&m, &l, 0) else { return Ok(V::Str(String::new())) };
            let start = idx as usize + utf16(&l).len();
            let end = if greedy {
                let mu = utf16(&m);
                let ru = utf16(&r);
                if ru.len() > mu.len() { None } else { (0..=mu.len() - ru.len()).rev().find(|&i| mu[i..i + ru.len()] == ru[..]) }
            } else {
                java_index_of(&m, &r, start as i64).map(|i| i as usize)
            };
            match end {
                Some(e) if e >= start && e <= su.len() => V::Str(String::from_utf16_lossy(&su[start..e])),
                Some(e) => {
                    return Err(EsError::java(500, "string_index_out_of_bounds_exception", format!("begin {start}, end {e}, length {}", su.len())));
                }
                None => V::Str(String::new()),
            }
        }
        "cidrmatch" => {
            let Some(ip) = str_arg(&a[0])? else { return Ok(V::Null) };
            let Ok(addr) = ip.parse::<std::net::IpAddr>() else { return Ok(V::Bool(false)) };
            let mut any = false;
            for c in &a[1..] {
                let Some(c) = str_arg(c)? else { continue };
                if cidr_contains(&c, addr)? {
                    any = true;
                    break;
                }
            }
            V::Bool(any)
        }
        "concat" => {
            let mut s = String::new();
            for v in &a {
                match v_to_string(v) {
                    Some(x) => s.push_str(&x),
                    None => return Ok(V::Null),
                }
            }
            V::Str(s)
        }
        "endswith" | "startswith" | "stringcontains" => {
            let (Some(s), Some(p)) = (str_arg(&a[0])?, str_arg(&a[1])?) else { return Ok(V::Null) };
            let (s, p) = (lower(&s), lower(&p));
            V::Bool(match name {
                "endswith" => s.ends_with(&p),
                "startswith" => s.starts_with(&p),
                _ => !s.is_empty() && !p.is_empty() && s.contains(&p),
            })
        }
        "indexof" => {
            let Some(s) = str_arg(&a[0])? else { return Ok(V::Null) };
            let Some(sub) = str_arg(&a[1])? else { return Ok(V::Null) };
            let start = int_arg(a.get(2))?.unwrap_or(0);
            match java_index_of(&lower(&s), &lower(&sub), start) {
                Some(i) => V::Long(i),
                None => V::Null,
            }
        }
        "length" => match str_arg(&a[0])? {
            Some(s) => V::Long(s.encode_utf16().count() as i64),
            None => V::Null,
        },
        "string" => match v_to_string(&a[0]) {
            Some(s) => V::Str(s),
            None => V::Null,
        },
        "substring" => {
            let Some(s) = str_arg(&a[0])? else { return Ok(V::Null) };
            let Some(start) = int_arg(a.get(1))? else { return Ok(V::Str(s)) };
            let u = utf16(&s);
            let len = u.len() as i64;
            let end = int_arg(a.get(2))?.unwrap_or(len);
            if u.is_empty() {
                return Ok(V::Str(s));
            }
            let st = (if start < 0 { start + len } else { start }).max(0);
            let en = (if end < 0 { end + len } else { end }).clamp(0, len);
            if st >= en { V::Str(String::new()) } else { V::Str(String::from_utf16_lossy(&u[st as usize..en as usize])) }
        }
        "add" => arith(ArithOp::Add, a[0].clone(), a[1].clone())?,
        "subtract" => arith(ArithOp::Sub, a[0].clone(), a[1].clone())?,
        "multiply" => arith(ArithOp::Mul, a[0].clone(), a[1].clone())?,
        "divide" => arith(ArithOp::Div, a[0].clone(), a[1].clone())?,
        "modulo" => arith(ArithOp::Mod, a[0].clone(), a[1].clone())?,
        "number" => {
            let Some(s) = str_arg(&a[0])? else { return Ok(V::Null) };
            let hex = s.starts_with("0x");
            let radix = match a.get(1) {
                None | Some(V::Null) => {
                    if hex {
                        16
                    } else {
                        10
                    }
                }
                Some(V::Long(r)) => *r,
                Some(other) => return Err(eval_err(format!("An integer base is required; received [{}]", v_to_string(other).unwrap_or_default()))),
            };
            let body = if hex && radix == 16 { &s[2..] } else { s.as_str() };
            let bad = || EsError::new(400, "invalid_argument_exception", format!("Unable to convert [{body}] to number of base [{radix}]"));
            if radix == 10 {
                if let Ok(i) = body.parse::<i64>() {
                    V::Long(i)
                } else {
                    let d: f64 = body.parse().map_err(|_| bad())?;
                    if d.is_finite() {
                        V::Double(d)
                    } else {
                        return Err(bad());
                    }
                }
            } else {
                if !(2..=36).contains(&radix) {
                    return Err(bad());
                }
                V::Long(i64::from_str_radix(body, radix as u32).map_err(|_| bad())?)
            }
        }
        other => return Err(eval_err(format!("Unknown function [{other}]"))),
    })
}

fn cidr_contains(cidr: &str, addr: std::net::IpAddr) -> EsResult<bool> {
    let bad = || EsError::illegal_argument(format!("Expected IP address and network mask in CIDR notation, got [{cidr}]"));
    let (net, bits) = match cidr.split_once('/') {
        Some((n, b)) => (n, Some(b.parse::<u32>().map_err(|_| bad())?)),
        None => (cidr, None),
    };
    let net: std::net::IpAddr = net.parse().map_err(|_| bad())?;
    let to_bytes = |ip: std::net::IpAddr| -> [u8; 16] {
        match ip {
            std::net::IpAddr::V4(v) => v.to_ipv6_mapped().octets(),
            std::net::IpAddr::V6(v) => v.octets(),
        }
    };
    let offset = if net.is_ipv4() { 96 } else { 0 };
    let bits = bits.map_or(128, |b| b + offset);
    if bits > 128 {
        return Err(bad());
    }
    let (a, n) = (to_bytes(addr), to_bytes(net));
    let full = (bits / 8) as usize;
    if a[..full] != n[..full] {
        return Ok(false);
    }
    let rem = bits % 8;
    if rem == 0 {
        return Ok(true);
    }
    let mask = 0xffu8 << (8 - rem);
    Ok(a[full] & mask == n[full] & mask)
}

fn eval_node(n: &Node, matched: &[bool], doc: &Doc) -> EsResult<bool> {
    Ok(match n {
        Node::And(a, b) => eval_node(a, matched, doc)? && eval_node(b, matched, doc)?,
        Node::Or(a, b) => eval_node(a, matched, doc)? || eval_node(b, matched, doc)?,
        Node::Not(x) => !eval_node(x, matched, doc)?,
        Node::Dsl(i) => matched.get(*i).copied().unwrap_or(false),
        Node::Mem(e) => matches!(eval(e, doc)?, V::Bool(true)),
        Node::Const(b) => *b,
    })
}

// --- stages --------------------------------------------------------------------------------------

/// A resolved and planned `[...]` (or the whole event query).
#[derive(Clone, Debug, PartialEq)]
struct Stage {
    category: Option<String>,
    cond: Cond,
    /// Join key fields (query names), with whether each is optional.
    keys: Vec<(String, bool)>,
    missing: bool,
}

struct Ctx<'a> {
    cluster: &'a Cluster,
    s: &'a Settings,
    schema: Schema,
    ts_type: FType,
    pit: Option<String>,
}

/// One event as the matching algorithms see it.
#[derive(Clone, Debug)]
struct Event {
    ts: i64,
    tb: Option<Value>,
    doc: i64,
    key: Vec<Value>,
    index: String,
    id: String,
}

impl Event {
    fn ord_cmp(&self, o: &Event) -> Ordering {
        self.ts
            .cmp(&o.ts)
            .then_with(|| match (&self.tb, &o.tb) {
                (Some(a), Some(b)) => value_cmp(a, b),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            .then(self.doc.cmp(&o.doc))
    }
}

fn value_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(i), Some(j)) => i.cmp(&j),
            _ => x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(Ordering::Equal),
        },
        (Value::Str(x), Value::Str(y)) => java_cmp(x, y),
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        _ => a.to_json().cmp(&b.to_json()),
    }
}

fn keys_cmp(a: &[Value], b: &[Value]) -> Ordering {
    a.iter().zip(b).map(|(x, y)| value_cmp(x, y)).find(|o| *o != Ordering::Equal).unwrap_or(Ordering::Equal)
}

fn breaker() -> EsError {
    EsError::new(429, "circuit_breaking_exception", format!("[eql_sequence] data too large, more than [{MAX_HELD_EVENTS}] events would be held for this query"))
        .with_meta("durability", "TRANSIENT")
}

impl Ctx<'_> {
    fn exact(&self, name: &str) -> String {
        self.schema.fields.get(name).and_then(|f| f.exact.clone()).unwrap_or_else(|| name.to_string())
    }

    /// The filter of a stage (`with_timestamp`: events must have a timestamp, as sequence
    /// stages need).
    fn stage_query(&self, st: &Stage, with_timestamp: bool) -> Value {
        let mut filters = Vec::new();
        if let Some(c) = &st.category {
            filters.push(obj! { "term" => obj! { self.exact(&self.s.event_category_field) => c.clone() } });
        }
        if with_timestamp {
            filters.push(obj! { "exists" => obj! { "field" => self.s.timestamp_field.clone() } });
        }
        if let Some(f) = &self.s.filter {
            filters.push(f.clone());
        }
        for (k, optional) in &st.keys {
            if !optional {
                filters.push(obj! { "exists" => obj! { "field" => self.exact(k) } });
            }
        }
        let mem = st.cond.has_mem();
        let cond = if mem { st.cond.superset(&st.cond.root) } else { Some(st.cond.exact_query(&st.cond.root)) };
        if let Some(c) = cond {
            filters.push(c);
        }
        let mut b = Map::new();
        b.insert("filter".into(), Value::Arr(filters));
        if mem && !st.cond.leaves.is_empty() {
            let named: Vec<Value> = st
                .cond
                .leaves
                .iter()
                .enumerate()
                .map(|(i, q)| obj! { "bool" => obj! { "filter" => crate::arr![q.clone()], "_name" => format!("_eql_leaf_{i}") } })
                .collect();
            b.insert("should".into(), Value::Arr(named));
            b.insert("minimum_should_match".into(), 0.into());
        }
        obj! { "bool" => Value::Obj(b) }
    }

    fn sort(&self, desc: bool) -> Value {
        let order = if desc { "desc" } else { "asc" };
        let mut s = vec![obj! { self.s.timestamp_field.clone() => obj! { "order" => order } }];
        if let Some(tb) = &self.s.tiebreaker_field {
            s.push(obj! { tb.clone() => obj! { "order" => order } });
        }
        s.push(obj! { "_shard_doc" => obj! { "order" => order } });
        Value::Arr(s)
    }

    async fn open_pit(&mut self) -> EsResult<()> {
        let uri = format!(
            "/{}/_pit?keep_alive=5m&ignore_unavailable={}&expand_wildcards={}",
            enc(&self.s.index),
            self.s.ignore_unavailable,
            enc(&self.s.expand_wildcards)
        );
        let r = internal::call(self.cluster, "POST", &uri, None).await?;
        self.pit = r.get("id").map(Value::to_es_string);
        Ok(())
    }

    async fn close_pit(&mut self) {
        if let Some(id) = self.pit.take() {
            let _ = internal::call(self.cluster, "DELETE", "/_pit", Some(&obj! { "id" => id })).await;
        }
    }

    /// One page of a search over the point in time.
    async fn page(&mut self, mut body: Map) -> EsResult<Vec<Value>> {
        if let Some(pit) = &self.pit {
            body.insert("pit".into(), obj! { "id" => pit.clone(), "keep_alive" => "5m" });
        }
        if let Some(rm) = &self.s.runtime_mappings {
            body.insert("runtime_mappings".into(), rm.clone());
        }
        body.insert("track_total_hits".into(), false.into());
        let r = internal::call(self.cluster, "POST", "/_search?allow_partial_search_results=false", Some(&Value::Obj(body))).await?;
        if let Some(id) = r.get("pit_id").map(Value::to_es_string) {
            self.pit = Some(id);
        }
        Ok(r.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default())
    }

    fn fields_spec(&self, fields: &[String]) -> Value {
        Value::Arr(
            fields
                .iter()
                .map(|f| match self.schema.fields.get(f).map(|i| i.ty) {
                    Some(FType::Date | FType::DateNanos) => obj! { "field" => f.clone(), "format" => "epoch_millis" },
                    _ => obj! { "field" => f.clone() },
                })
                .collect(),
        )
    }

    /// The doc an evaluated leaf sees.
    fn doc_of(&self, hit: &Value, cond: &Cond) -> Doc {
        let mut d = Doc::default();
        for f in &cond.mem_fields {
            let mut vals: Vec<V> = hit.pointer(&["fields", f.as_str()]).and_then(Value::as_array).map(|a| a.iter().map(json_to_v).collect()).unwrap_or_default();
            if matches!(self.schema.fields.get(f).map(|i| i.ty), Some(FType::Date | FType::DateNanos)) {
                vals = vals
                    .into_iter()
                    .map(|v| match v {
                        V::Str(s) => s.parse::<i64>().map(V::Long).unwrap_or(V::Str(s)),
                        o => o,
                    })
                    .collect();
            }
            // Doc values are sorted.
            vals.sort_by(|a, b| v_cmp(a, b).unwrap_or(Ordering::Equal));
            d.values.insert(f.clone(), vals);
        }
        for (name, info) in &self.schema.fields {
            if let Some(e) = &info.exact
                && e != name
            {
                d.exact.insert(name.clone(), e.clone());
            }
        }
        d
    }

    fn matches(&self, hit: &Value, cond: &Cond) -> EsResult<bool> {
        if !cond.has_mem() {
            return Ok(true);
        }
        let mut matched = vec![false; cond.leaves.len()];
        let names: Vec<String> = match hit.get("matched_queries") {
            Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
            Some(Value::Obj(m)) => m.keys().cloned().collect(),
            _ => Vec::new(),
        };
        for n in names {
            if let Some(i) = n.strip_prefix("_eql_leaf_").and_then(|x| x.parse::<usize>().ok())
                && i < matched.len()
            {
                matched[i] = true;
            }
        }
        eval_node(&cond.root, &matched, &self.doc_of(hit, cond))
    }

    fn ts_nanos(&self, v: Option<&Value>) -> i64 {
        let raw = match v {
            Some(Value::Num(n)) => n.as_i64().unwrap_or(n.as_f64() as i64),
            Some(Value::Str(s)) => s.parse::<i64>().unwrap_or(0),
            _ => return i64::MIN,
        };
        match self.ts_type {
            FType::DateNanos => raw,
            _ => raw.saturating_mul(1_000_000),
        }
    }

    fn key_of(&self, hit: &Value, keys: &[(String, bool)]) -> Vec<Value> {
        keys.iter()
            .map(|(k, _)| {
                let f = self.exact(k);
                hit.pointer(&["fields", f.as_str()]).and_then(Value::as_array).and_then(|a| a.first()).cloned().unwrap_or(Value::Null)
            })
            .collect()
    }

    fn event_of(&self, hit: &Value, keys: &[(String, bool)]) -> Event {
        let sort = hit.get("sort").and_then(Value::as_array);
        let at = |i: usize| sort.and_then(|s| s.get(i));
        let has_tb = self.s.tiebreaker_field.is_some();
        let tb = if has_tb { at(1).filter(|v| !matches!(v, Value::Null)).cloned() } else { None };
        let doc = at(if has_tb { 2 } else { 1 }).and_then(Value::as_i64).unwrap_or(0);
        Event {
            ts: self.ts_nanos(at(0)),
            tb,
            doc,
            key: self.key_of(hit, keys),
            index: hit.get("_index").map(Value::to_es_string).unwrap_or_default(),
            id: hit.get("_id").map(Value::to_es_string).unwrap_or_default(),
        }
    }

    /// Every matching hit of a stage, in `sort` order, as `f(hit)` sees them; `f` returns
    /// `false` to stop.
    async fn scan(&mut self, query: &Value, sort: Value, fields: &[String], mut f: impl FnMut(&Self, &Value) -> EsResult<bool>) -> EsResult<()> {
        let page_size = self.s.fetch_size.clamp(2, MAX_PAGE);
        let mut after: Option<Value> = None;
        loop {
            let mut body = Map::new();
            body.insert("size".into(), page_size.into());
            body.insert("query".into(), query.clone());
            body.insert("sort".into(), sort.clone());
            body.insert("_source".into(), false.into());
            body.insert("fields".into(), self.fields_spec(fields));
            if let Some(a) = &after {
                body.insert("search_after".into(), a.clone());
            }
            let hits = self.page(body).await?;
            for h in &hits {
                if !f(self, h)? {
                    return Ok(());
                }
            }
            if hits.len() < page_size {
                return Ok(());
            }
            after = hits.last().and_then(|h| h.get("sort")).cloned();
            if after.is_none() {
                return Ok(());
            }
        }
    }

    fn stage_fields(&self, st: &Stage) -> Vec<String> {
        let mut fields: Vec<String> = st.keys.iter().map(|(k, _)| self.exact(k)).collect();
        for f in &st.cond.mem_fields {
            if !fields.contains(f) {
                fields.push(f.clone());
            }
        }
        fields
    }

    /// Every event of a stage, ascending (keys and ordinals only).
    async fn stage_events(&mut self, st: &Stage, held: &mut usize) -> EsResult<Vec<Event>> {
        let query = self.stage_query(st, true);
        let fields = self.stage_fields(st);
        let sort = self.sort(false);
        let mut out = Vec::new();
        self.scan(&query, sort, &fields, |ctx, h| {
            if ctx.matches(h, &st.cond)? {
                out.push(ctx.event_of(h, &st.keys));
                *held += 1;
                if *held > MAX_HELD_EVENTS {
                    return Err(breaker());
                }
            }
            Ok(true)
        })
        .await?;
        Ok(out)
    }

    /// `_source` (and the requested `fields`) of events, by `(index, id)`.
    async fn fetch(&self, refs: &[(String, String)]) -> EsResult<HashMap<(String, String), Value>> {
        let mut out = HashMap::new();
        let mut by_index: Vec<(String, Vec<String>)> = Vec::new();
        for (i, id) in refs {
            match by_index.iter_mut().find(|(x, _)| x == i) {
                Some((_, ids)) => {
                    if !ids.contains(id) {
                        ids.push(id.clone());
                    }
                }
                None => by_index.push((i.clone(), vec![id.clone()])),
            }
        }
        for (index, ids) in &by_index {
            for chunk in ids.chunks(MAX_PAGE) {
                let mut body = Map::new();
                body.insert("size".into(), chunk.len().into());
                body.insert(
                    "query".into(),
                    obj! { "bool" => obj! { "filter" => crate::arr![
                        obj! { "term" => obj! { "_index" => index.clone() } },
                        obj! { "ids" => obj! { "values" => Value::Arr(chunk.iter().map(|x| Value::from(x.clone())).collect()) } }
                    ] } },
                );
                body.insert("_source".into(), true.into());
                body.insert("track_total_hits".into(), false.into());
                if let Some(f) = &self.s.fields {
                    body.insert("fields".into(), f.clone());
                }
                if let Some(rm) = &self.s.runtime_mappings {
                    body.insert("runtime_mappings".into(), rm.clone());
                }
                let uri = format!("/{}/_search?ignore_unavailable=true&allow_no_indices=true&expand_wildcards=all", enc(index));
                let r = internal::call(self.cluster, "POST", &uri, Some(&Value::Obj(body))).await?;
                for h in r.pointer(&["hits", "hits"]).and_then(Value::as_array).into_iter().flatten() {
                    let key = (h.get("_index").map(Value::to_es_string).unwrap_or_default(), h.get("_id").map(Value::to_es_string).unwrap_or_default());
                    out.insert(key, event_json(h));
                }
            }
        }
        Ok(out)
    }
}

/// An event as EQL responses render it.
fn event_json(h: &Value) -> Value {
    let mut m = Map::new();
    m.insert("_index".into(), h.get("_index").cloned().unwrap_or(Value::Null));
    m.insert("_id".into(), h.get("_id").cloned().unwrap_or(Value::Null));
    if let Some(s) = h.get("_source") {
        m.insert("_source".into(), s.clone());
    }
    if let Some(f) = h.get("fields") {
        m.insert("fields".into(), f.clone());
    }
    Value::Obj(m)
}

fn missing_event() -> Value {
    obj! { "_index" => "", "_id" => "", "_source" => obj! {}, "missing" => true }
}

// --- limits ----------------------------------------------------------------------------------------

/// Elasticsearch's combined limit: a signed limit (negative: tail) and an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub limit: i64,
    pub offset: i64,
}

/// The default limit plus the pipes, combined like `CombineLimits`.
pub fn combine_limits(size: i32, tail_default: bool, pipes: &[Pipe]) -> Limit {
    let size = size as i64;
    let signed = |p: &Pipe| match p {
        Pipe::Head(n) => *n as i64,
        Pipe::Tail(n) => -(*n as i64),
    };
    // The implicit limit takes the first pipe's kind.
    let mut lim = match pipes.first() {
        Some(Pipe::Head(_)) => Limit { limit: size, offset: 0 },
        Some(Pipe::Tail(_)) => Limit { limit: -size, offset: 0 },
        None => Limit { limit: if tail_default { -size } else { size }, offset: 0 },
    };
    for p in pipes {
        let secondary = signed(p);
        let primary = lim.limit;
        let sign = primary.signum();
        if primary > 0 && secondary > 0 {
            lim.limit = primary.min(secondary);
        } else if primary < 0 && secondary < 0 {
            lim.limit = primary.max(secondary);
        } else if secondary.abs() < primary.abs() {
            lim.offset += (primary + secondary).abs();
            lim.limit = secondary.abs() * sign;
        }
    }
    lim
}

/// `Limit.view` over an ascending list, presented ascending.
fn limit_view<T: Clone>(values: &[T], lim: Limit) -> Vec<T> {
    if values.is_empty() || lim.limit == 0 {
        return Vec::new();
    }
    let mut v: Vec<T> = values.to_vec();
    if lim.limit < 0 {
        v.reverse();
    }
    let total = (lim.limit.abs() + lim.offset) as usize;
    let abs = lim.limit.unsigned_abs() as usize;
    let size = v.len();
    let mut out = if size >= total {
        v[lim.offset as usize..total].to_vec()
    } else if size <= abs {
        v
    } else {
        v[size - abs..].to_vec()
    };
    if lim.limit < 0 {
        out.reverse();
    }
    out
}

// --- planning and running ----------------------------------------------------------------------------

fn plan_stage(f: &EventFilter, keys: &[Expr], missing: bool, schema: &Schema, v: &mut Verifier) -> EsResult<Stage> {
    let (cond, ty) = v.resolve(&f.condition)?;
    if let Some(t) = &ty
        && !matches!(t, SType::Null)
        && !t.is_bool()
    {
        let name = match t {
            SType::Field(ft) => type_display(ft).to_uppercase(),
            other => other.name().to_uppercase(),
        };
        v.problem(f.condition.start, format!("Condition expression needs to be boolean, found [{name}]"));
    }
    let mut key_list = Vec::new();
    for k in keys {
        if let ExprKind::Field { name, optional } = &k.kind {
            if !schema.fields.contains_key(name) && !optional {
                v.problem(k.start, format!("Unknown column [{name}]"));
            }
            key_list.push((name.clone(), *optional));
        }
    }
    let mut p = Planner { schema, leaves: Vec::new(), mem_fields: Vec::new() };
    let root = p.plan(&cond);
    Ok(Stage { category: f.category.clone(), cond: Cond { root, leaves: p.leaves, mem_fields: p.mem_fields }, keys: key_list, missing })
}

enum Plan {
    Event(Stage),
    Sequence { stages: Vec<Stage>, until: Option<Stage>, maxspan: Option<i64> },
    Sample(Vec<Stage>),
}

/// Verifies and runs a statement.
pub async fn run(cluster: &Cluster, src: &str, stmt: &Statement, s: &Settings) -> EsResult<Output> {
    let schema = schema(cluster, s).await?;
    let mut v = Verifier { src, schema: &schema, problems: Vec::new() };
    // The timestamp, tiebreaker and category fields must exist.
    let needs_category = match &stmt.query {
        Query::Event(f) => f.category.is_some(),
        Query::Sequence { terms, until, .. } => terms.iter().chain(until).any(|t| t.filter.category.is_some()),
        Query::Sample { terms } => terms.iter().any(|t| t.filter.category.is_some()),
    };
    if !matches!(stmt.query, Query::Sample { .. }) && !schema.fields.contains_key(&s.timestamp_field) {
        v.problem(usize::MAX, format!("Unknown column [{}]", s.timestamp_field));
    }
    if let Some(tb) = &s.tiebreaker_field
        && !schema.fields.contains_key(tb)
    {
        v.problem(usize::MAX, format!("Unknown column [{tb}]"));
    }
    if needs_category && !schema.fields.contains_key(&s.event_category_field) {
        v.problem(0, format!("Unknown column [{}]", s.event_category_field));
    }
    let plan_term = |t: &Term, v: &mut Verifier| plan_stage(&t.filter, &t.keys, t.missing, &schema, v);
    let plan = match &stmt.query {
        Query::Event(f) => Plan::Event(plan_stage(f, &[], false, &schema, &mut v)?),
        Query::Sequence { terms, until, maxspan_nanos } => {
            let mut stages = Vec::new();
            for t in terms {
                stages.push(plan_term(t, &mut v)?);
            }
            let until = match until {
                Some(t) => Some(plan_term(t, &mut v)?),
                None => None,
            };
            Plan::Sequence { stages, until, maxspan: *maxspan_nanos }
        }
        Query::Sample { terms } => {
            let mut stages = Vec::new();
            for t in terms {
                stages.push(plan_term(t, &mut v)?);
            }
            Plan::Sample(stages)
        }
    };
    v.finish()?;
    let ts_type = schema.fields.get(&s.timestamp_field).map_or(FType::Date, |f| f.ty);
    let mut ctx = Ctx { cluster, s, schema, ts_type, pit: None };
    ctx.open_pit().await?;
    let lim = combine_limits(s.size, s.tail, &stmt.pipes);
    let result = match &plan {
        Plan::Event(st) => events(&mut ctx, st, lim).await,
        Plan::Sequence { stages, until, maxspan } => sequences(&mut ctx, stages, until.as_ref(), *maxspan, lim).await,
        Plan::Sample(stages) => samples(&mut ctx, stages).await,
    };
    ctx.close_pit().await;
    result
}

async fn events(ctx: &mut Ctx<'_>, st: &Stage, lim: Limit) -> EsResult<Output> {
    let need = (lim.limit.unsigned_abs() + lim.offset.max(0) as u64) as usize;
    if lim.limit == 0 {
        return Ok(Output::Events(Vec::new()));
    }
    // Elasticsearch reads in the limit's direction.
    let desc = lim.limit < 0;
    let query = ctx.stage_query(st, false);
    let mem = st.cond.has_mem();
    let mut selected: Vec<(String, String)> = Vec::new();
    if mem {
        let fields = st.cond.mem_fields.clone();
        let sort = ctx.sort(desc);
        ctx.scan(&query, sort, &fields, |c, h| {
            if c.matches(h, &st.cond)? {
                selected.push((h.get("_index").map(Value::to_es_string).unwrap_or_default(), h.get("_id").map(Value::to_es_string).unwrap_or_default()));
            }
            Ok(selected.len() < need)
        })
        .await?;
    } else {
        // One query does it: read just what the limit keeps, with sources.
        let mut out = Vec::new();
        let mut after: Option<Value> = None;
        while out.len() < need {
            let size = (need - out.len()).min(MAX_PAGE);
            let mut body = Map::new();
            body.insert("size".into(), size.into());
            body.insert("query".into(), query.clone());
            body.insert("sort".into(), ctx.sort(desc));
            body.insert("_source".into(), true.into());
            if let Some(f) = &ctx.s.fields {
                body.insert("fields".into(), f.clone());
            }
            if let Some(a) = &after {
                body.insert("search_after".into(), a.clone());
            }
            let hits = ctx.page(body).await?;
            let n = hits.len();
            after = hits.last().and_then(|h| h.get("sort")).cloned();
            out.extend(hits);
            if n < size || after.is_none() {
                break;
            }
        }
        let mut chosen: Vec<Value> = out.iter().skip(lim.offset.max(0) as usize).map(event_json).collect();
        if desc {
            chosen.reverse();
        }
        return Ok(Output::Events(chosen));
    }
    let mut refs: Vec<(String, String)> = selected.into_iter().skip(lim.offset.max(0) as usize).collect();
    if desc {
        refs.reverse();
    }
    let docs = ctx.fetch(&refs).await?;
    Ok(Output::Events(refs.iter().filter_map(|r| docs.get(r).cloned()).collect()))
}

/// An in-flight or completed sequence.
#[derive(Clone, Debug)]
struct Seq {
    key: Vec<Value>,
    /// The matched event of each stage (`None` for missing stages).
    events: Vec<Option<usize>>,
    first: usize,
    last: usize,
}

/// A stage's events grouped by join key.
type KeyGroups = HashMap<String, Vec<usize>>;

fn key_string(k: &[Value]) -> String {
    let mut s = String::new();
    for v in k {
        s.push_str(&v.to_json());
        s.push('\u{1}');
    }
    s
}

fn group(pool: &[Event], range: std::ops::Range<usize>) -> KeyGroups {
    let mut g: KeyGroups = HashMap::new();
    for i in range {
        g.entry(key_string(&pool[i].key)).or_default().push(i);
    }
    g
}

async fn sequences(ctx: &mut Ctx<'_>, stages: &[Stage], until: Option<&Stage>, maxspan: Option<i64>, lim: Limit) -> EsResult<Output> {
    let mut held = 0usize;
    let n = stages.len();
    // Repeated terms (`with runs=N`) share their events.
    let mut pool: Vec<Event> = Vec::new();
    let mut stage_events: Vec<std::ops::Range<usize>> = Vec::with_capacity(n);
    for (i, st) in stages.iter().enumerate() {
        if st.missing {
            stage_events.push(0..0);
            continue;
        }
        if let Some(j) = (0..i).find(|j| !stages[*j].missing && stages[*j] == *st) {
            let r = stage_events[j].clone();
            stage_events.push(r);
            continue;
        }
        let base = pool.len();
        pool.extend(ctx.stage_events(st, &mut held).await?);
        stage_events.push(base..pool.len());
    }
    let until_groups = match until {
        Some(u) => {
            let base = pool.len();
            pool.extend(ctx.stage_events(u, &mut held).await?);
            Some(group(&pool, base..pool.len()))
        }
        None => None,
    };
    let mut missing_groups: Vec<Option<KeyGroups>> = vec![None; n];
    for (i, st) in stages.iter().enumerate() {
        if st.missing {
            let base = pool.len();
            pool.extend(ctx.stage_events(st, &mut held).await?);
            missing_groups[i] = Some(group(&pool, base..pool.len()));
        }
    }
    let positive: Vec<usize> = (0..n).filter(|i| !stages[*i].missing).collect();
    let first_pos = positive[0];
    let last_pos = *positive.last().expect("a sequence has a positive stage");
    let exceeds = |from: i64, to: i64| maxspan.is_some_and(|m| m > 0 && to.saturating_sub(from) > m);
    let mut candidates: Vec<Seq> = Vec::new();
    // In-flight sequences of the previous positive stage, per key, ordered by last event.
    let mut inflight: HashMap<String, Vec<Seq>> = HashMap::new();
    for e in stage_events[first_pos].clone() {
        let mut events = vec![None; n];
        events[first_pos] = Some(e);
        let seq = Seq { key: pool[e].key.clone(), events, first: e, last: e };
        if first_pos == last_pos {
            candidates.push(seq);
        } else {
            inflight.entry(key_string(&pool[e].key)).or_default().push(seq);
        }
    }
    for w in positive.windows(2) {
        let stage = w[1];
        let mut next: HashMap<String, Vec<Seq>> = HashMap::new();
        for e in stage_events[stage].clone() {
            let ev = &pool[e];
            let k = key_string(&ev.key);
            let Some(group) = inflight.get_mut(&k) else { continue };
            // The latest in-flight sequence before this event; it and the older ones go.
            let pos = group.partition_point(|s| pool[s.last].ord_cmp(ev) == Ordering::Less);
            if pos == 0 {
                continue;
            }
            let mut seq = group[pos - 1].clone();
            group.drain(..pos);
            if group.is_empty() {
                inflight.remove(&k);
            }
            if exceeds(pool[seq.first].ts, ev.ts) {
                continue;
            }
            if let Some(us) = until_groups.as_ref().and_then(|g| g.get(&k)) {
                // The nearest until event before this one rejects the match when it falls
                // between the sequence's last event and this one.
                let p = us.partition_point(|u| pool[*u].ord_cmp(ev) == Ordering::Less);
                if p > 0 {
                    let u = &pool[us[p - 1]];
                    if u.ord_cmp(&pool[seq.last]) != Ordering::Less {
                        continue;
                    }
                }
            }
            seq.events[stage] = Some(e);
            seq.last = e;
            if stage == last_pos {
                candidates.push(seq);
            } else {
                next.entry(k).or_default().push(seq);
            }
        }
        inflight = next;
    }
    // Missing events: a sequence survives when no event of a missing stage (same key) falls
    // where it must be absent (Elasticsearch's `TumblingWindow.doCheckMissingEvents`).
    if stages.iter().any(|s| s.missing) {
        let millis = |nanos: i64| nanos.div_euclid(1_000_000);
        candidates.retain(|seq| {
            let k = key_string(&seq.key);
            let mut last_leading: Option<i64> = None;
            let mut first_trailing: Option<i64> = None;
            for (i, groups) in missing_groups.iter().enumerate() {
                let Some(groups) = groups else { continue };
                let evs: &[usize] = groups.get(&k).map(Vec::as_slice).unwrap_or(&[]);
                if i < first_pos {
                    let start = millis(pool[seq.first].ts);
                    if let Some(t) = evs.iter().map(|e| pool[*e].ts).filter(|t| millis(*t) < start).max() {
                        last_leading = Some(last_leading.map_or(t, |l| l.max(t)));
                    }
                } else if i > last_pos {
                    let end = millis(pool[seq.last].ts);
                    if let Some(t) = evs.iter().map(|e| pool[*e].ts).filter(|t| millis(*t) > end).min() {
                        first_trailing = Some(first_trailing.map_or(t, |f| f.min(t)));
                    }
                } else {
                    let prev = (0..i).rev().find(|j| !stages[*j].missing).and_then(|j| seq.events[j]).map(|e| millis(pool[e].ts));
                    let nxt = (i + 1..n).find(|j| !stages[*j].missing).and_then(|j| seq.events[j]).map(|e| millis(pool[e].ts));
                    if let (Some(p), Some(nx)) = (prev, nxt)
                        && evs.iter().any(|e| {
                            let t = millis(pool[*e].ts);
                            t > p && t < nx
                        })
                    {
                        return false;
                    }
                }
            }
            let big = |from: Option<i64>, to: Option<i64>| match (from, to) {
                (Some(f), Some(t)) => exceeds(f, t),
                _ => true,
            };
            let (m0, ml) = (stages[0].missing, stages[n - 1].missing);
            (first_trailing.is_none() && last_leading.is_none())
                || (last_leading.is_none() && m0)
                || (first_trailing.is_none() && ml)
                || (m0 && ml && big(last_leading, first_trailing))
                || (m0 && !ml && big(last_leading, Some(pool[seq.last].ts)))
                || (!m0 && ml && big(Some(pool[seq.first].ts), first_trailing))
        });
    }
    // Completed sequences are ordered by their last event, then key.
    candidates.sort_by(|a, b| pool[a.last].ord_cmp(&pool[b.last]).then_with(|| keys_cmp(&a.key, &b.key)));
    let chosen = limit_view(&candidates, lim);
    let refs: Vec<(String, String)> = chosen.iter().flat_map(|s| s.events.iter().flatten().map(|e| (pool[*e].index.clone(), pool[*e].id.clone()))).collect();
    let docs = ctx.fetch(&refs).await?;
    let has_keys = stages.first().is_some_and(|s| !s.keys.is_empty());
    let out = chosen
        .iter()
        .map(|seq| {
            let mut m = Map::new();
            if has_keys {
                m.insert("join_keys".into(), Value::Arr(seq.key.clone()));
            }
            let events: Vec<Value> = seq
                .events
                .iter()
                .map(|e| match e {
                    Some(e) => docs.get(&(pool[*e].index.clone(), pool[*e].id.clone())).cloned().unwrap_or_else(missing_event),
                    None => missing_event(),
                })
                .collect();
            m.insert("events".into(), Value::Arr(events));
            Value::Obj(m)
        })
        .collect();
    Ok(Output::Sequences(out))
}

/// A join key's values and the `(index, id)` of the events that had it.
type KeyEvents = (Vec<Value>, Vec<(String, String)>);

async fn samples(ctx: &mut Ctx<'_>, stages: &[Stage]) -> EsResult<Output> {
    let limit = ctx.s.size.max(0) as usize;
    if limit == 0 {
        return Ok(Output::Sequences(Vec::new()));
    }
    let per_key = ctx.s.max_samples_per_key.max(1);
    // Each stage needs at most `stages * per_key` events per key to build `per_key` samples
    // of distinct events.
    let keep = stages.len() * per_key;
    let mut held = 0usize;
    let mut groups: Vec<HashMap<String, KeyEvents>> = Vec::with_capacity(stages.len());
    for st in stages {
        let query = ctx.stage_query(st, false);
        let fields = ctx.stage_fields(st);
        let mut g: HashMap<String, KeyEvents> = HashMap::new();
        ctx.scan(&query, crate::arr![obj! { "_shard_doc" => obj! { "order" => "asc" } }], &fields, |c, h| {
            if !c.matches(h, &st.cond)? {
                return Ok(true);
            }
            let key = c.key_of(h, &st.keys);
            let entry = g.entry(key_string(&key)).or_insert_with(|| (key, Vec::new()));
            if entry.1.len() < keep {
                entry.1.push((h.get("_index").map(Value::to_es_string).unwrap_or_default(), h.get("_id").map(Value::to_es_string).unwrap_or_default()));
                held += 1;
                if held > MAX_HELD_EVENTS {
                    return Err(breaker());
                }
            }
            Ok(true)
        })
        .await?;
        groups.push(g);
    }
    // Keys present in every stage, in composite-aggregation order.
    let mut keys: Vec<(String, Vec<Value>)> = groups[0].iter().filter(|(k, _)| groups.iter().all(|g| g.contains_key(*k))).map(|(k, (kv, _))| (k.clone(), kv.clone())).collect();
    keys.sort_by(|a, b| keys_cmp(&a.1, &b.1));
    let mut found: Vec<KeyEvents> = Vec::new();
    'keys: for (k, kv) in &keys {
        let lists: Vec<&Vec<(String, String)>> = groups.iter().map(|g| &g[k].1).collect();
        let mut result = Vec::new();
        sample_match(0, &lists, &mut result, &mut Vec::new(), per_key);
        for r in result {
            found.push((kv.clone(), r));
            if found.len() == limit {
                break 'keys;
            }
        }
    }
    let refs: Vec<(String, String)> = found.iter().flat_map(|(_, r)| r.iter().cloned()).collect();
    let docs = ctx.fetch(&refs).await?;
    Ok(Output::Sequences(
        found
            .into_iter()
            .map(|(k, r)| {
                let events: Vec<Value> = r.iter().filter_map(|x| docs.get(x).cloned()).collect();
                obj! { "join_keys" => Value::Arr(k), "events" => Value::Arr(events) }
            })
            .collect(),
    ))
}

/// `SampleIterator.match`: one distinct event per stage, depth first, up to `max` samples.
fn sample_match(stage: usize, lists: &[&Vec<(String, String)>], result: &mut Vec<Vec<(String, String)>>, partial: &mut Vec<(String, String)>, max: usize) {
    for r in lists[stage] {
        if partial.contains(r) {
            continue;
        }
        partial.push(r.clone());
        if stage == lists.len() - 1 {
            result.push(partial.clone());
        } else {
            sample_match(stage + 1, lists, result, partial, max);
        }
        if result.len() == max {
            return;
        }
        partial.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_combine_like_elasticsearch() {
        let l = |size, tail, pipes: &[Pipe]| combine_limits(size, tail, pipes);
        assert_eq!(l(10, true, &[]), Limit { limit: -10, offset: 0 });
        assert_eq!(l(10, false, &[]), Limit { limit: 10, offset: 0 });
        assert_eq!(l(10, true, &[Pipe::Head(3)]), Limit { limit: 3, offset: 0 });
        assert_eq!(l(10, true, &[Pipe::Head(3), Pipe::Tail(2)]), Limit { limit: 2, offset: 1 });
        assert_eq!(l(10, true, &[Pipe::Tail(5), Pipe::Tail(2), Pipe::Head(1)]), Limit { limit: -1, offset: 1 });
        assert_eq!(l(10, true, &[Pipe::Tail(20), Pipe::Head(2)]), Limit { limit: -2, offset: 8 });
    }

    #[test]
    fn views_match_elasticsearch() {
        // Six sequences [1-2] .. [6-7] (captured from Elasticsearch 9.5.5).
        let seqs: Vec<i32> = (1..=6).collect();
        let v = |pipes: &[Pipe], size, tail| limit_view(&seqs, combine_limits(size, tail, pipes));
        assert_eq!(v(&[], 2, true), vec![5, 6]);
        assert_eq!(v(&[], 2, false), vec![1, 2]);
        assert_eq!(v(&[Pipe::Head(2)], 10, true), vec![1, 2]);
        assert_eq!(v(&[Pipe::Tail(1)], 10, true), vec![6]);
        assert_eq!(v(&[Pipe::Tail(3), Pipe::Head(1)], 10, true), vec![4]);
        assert_eq!(v(&[Pipe::Head(3), Pipe::Tail(1)], 10, true), vec![3]);
        assert_eq!(v(&[Pipe::Head(4), Pipe::Tail(2)], 10, true), vec![3, 4]);
        assert_eq!(v(&[Pipe::Tail(4), Pipe::Head(2)], 10, true), vec![3, 4]);
        assert_eq!(v(&[Pipe::Tail(20), Pipe::Head(2)], 10, true), vec![1, 2]);
        assert_eq!(v(&[Pipe::Tail(2)], 10, false), vec![5, 6]);
        // Seven events: `| tail 3 | head 2` is [5, 6] on Elasticsearch.
        let evs: Vec<i32> = (1..=7).collect();
        assert_eq!(limit_view(&evs, combine_limits(10, true, &[Pipe::Tail(3), Pipe::Head(2)])), vec![5, 6]);
    }

    fn doc(pairs: &[(&str, V)]) -> Doc {
        let mut d = Doc::default();
        for (k, v) in pairs {
            d.values.insert(k.to_string(), vec![v.clone()]);
        }
        d
    }

    fn ev(q: &str, d: &Doc) -> V {
        let stmt = super::super::parser::parse(&format!("any where {q}")).unwrap();
        let Query::Event(f) = stmt.query else { panic!() };
        // Function names are lowercased by verification.
        fn lower(e: &mut Expr) {
            match &mut e.kind {
                ExprKind::Call { name, args, .. } => {
                    *name = name.to_ascii_lowercase();
                    args.iter_mut().for_each(lower);
                }
                ExprKind::Not(x) | ExprKind::Neg(x) => lower(x),
                ExprKind::And(a, b) | ExprKind::Or(a, b) | ExprKind::Cmp(_, a, b) | ExprKind::Arith(_, a, b) => {
                    lower(a);
                    lower(b);
                }
                ExprKind::In { expr, list, .. } => {
                    lower(expr);
                    list.iter_mut().for_each(lower);
                }
                ExprKind::Match { expr, .. } => lower(expr),
                _ => {}
            }
        }
        let mut c = f.condition;
        lower(&mut c);
        eval(&c, d).unwrap()
    }

    #[test]
    fn functions() {
        let d = doc(&[("s", V::Str("Hello World".into())), ("n", V::Long(5)), ("ip", V::Str("10.1.2.3".into()))]);
        assert_eq!(ev("between(s, \"He\", \"o\")", &d), V::Str("ll".into()));
        assert_eq!(ev("between(s, \"He\", \"o\", true)", &d), V::Str("llo W".into()));
        assert_eq!(ev("between~(s, \"he\", \"O\")", &d), V::Str("ll".into()));
        assert_eq!(ev("indexOf(s, \"o\")", &d), V::Long(4));
        assert_eq!(ev("indexOf(s, \"o\", 5)", &d), V::Long(7));
        assert_eq!(ev("indexOf(s, \"z\")", &d), V::Null);
        assert_eq!(ev("substring(s, -5)", &d), V::Str("World".into()));
        assert_eq!(ev("substring(s, 0, -6)", &d), V::Str("Hello".into()));
        assert_eq!(ev("length(s)", &d), V::Long(11));
        assert_eq!(ev("concat(s, \"!\", n)", &d), V::Str("Hello World!5".into()));
        assert_eq!(ev("concat(s, null)", &d), V::Null);
        assert_eq!(ev("startsWith~(s, \"hello\")", &d), V::Bool(true));
        assert_eq!(ev("startsWith(s, \"hello\")", &d), V::Bool(false));
        assert_eq!(ev("stringContains(s, \"lo W\")", &d), V::Bool(true));
        assert_eq!(ev("number(\"0x1F\")", &d), V::Long(31));
        assert_eq!(ev("number(\"1.5\")", &d), V::Double(1.5));
        assert_eq!(ev("number(\"11\", 2)", &d), V::Long(3));
        assert_eq!(ev("cidrMatch(ip, \"10.0.0.0/8\")", &d), V::Bool(true));
        assert_eq!(ev("cidrMatch(ip, \"192.168.0.0/16\", \"10.1.2.3\")", &d), V::Bool(true));
        assert_eq!(ev("cidrMatch(ip, \"192.168.0.0/16\")", &d), V::Bool(false));
        assert_eq!(ev("n + 1 > 5", &d), V::Bool(true));
        assert_eq!(ev("n / 2 == 2", &d), V::Bool(true));
        assert_eq!(ev("modulo(n, 3) == 2", &d), V::Bool(true));
        assert_eq!(ev("missing == null", &d), V::Bool(true));
        assert_eq!(ev("missing + 1 > 0", &d), V::Null);
        assert_eq!(ev("s : \"hello*\"", &d), V::Bool(true));
        assert_eq!(ev("s like \"hello*\"", &d), V::Bool(false));
        assert_eq!(ev("s like~ \"hello*\"", &d), V::Bool(true));
        assert_eq!(ev("s regex \"H.*d\"", &d), V::Bool(true));
        assert_eq!(ev("n in (1, 5)", &d), V::Bool(true));
        assert_eq!(ev("n in (1, null)", &d), V::Null);
        assert_eq!(ev("s in~ (\"hello world\")", &d), V::Bool(true));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("a*c", "abbbc", false));
        assert!(wildcard_match("a?c", "abc", false));
        assert!(!wildcard_match("a?c", "abbc", false));
        assert!(wildcard_match("A*", "abc", true));
        assert_eq!(lucene_wildcard(r"C:\Windows\*"), r"C:\\Windows\\*");
    }

    fn e(ts: i64, doc: i64) -> Event {
        Event { ts, tb: None, doc, key: vec![], index: "i".into(), id: doc.to_string() }
    }

    #[test]
    fn ordinals() {
        assert_eq!(e(1, 5).ord_cmp(&e(2, 0)), Ordering::Less);
        assert_eq!(e(1, 5).ord_cmp(&e(1, 6)), Ordering::Less);
        let mut a = e(1, 9);
        a.tb = Some(Value::from(1));
        // A null tiebreaker sorts last.
        assert_eq!(a.ord_cmp(&e(1, 0)), Ordering::Less);
    }

    #[test]
    fn samples_pick_distinct_events() {
        let l = |ids: &[&str]| ids.iter().map(|i| ("x".to_string(), i.to_string())).collect::<Vec<_>>();
        let (a, b) = (l(&["1", "2"]), l(&["1", "3"]));
        let mut result = Vec::new();
        sample_match(0, &[&a, &b], &mut result, &mut Vec::new(), 10);
        let ids: Vec<Vec<&str>> = result.iter().map(|r| r.iter().map(|(_, i)| i.as_str()).collect()).collect();
        assert_eq!(ids, vec![vec!["1", "3"], vec!["2", "1"], vec!["2", "3"]]);
        let mut one = Vec::new();
        sample_match(0, &[&a, &b], &mut one, &mut Vec::new(), 1);
        assert_eq!(one.len(), 1);
    }
}
