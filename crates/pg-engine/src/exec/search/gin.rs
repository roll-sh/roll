//! GIN's search structure over arrays, jsonb and tsvector (`array_ops`, intarray's
//! `gin__int_ops`, `jsonb_ops`, `jsonb_path_ops`, `tsvector_ops`): a posting list of rows
//! per element, as GIN keys them (an array's elements, a jsonb value's keys and scalars, a
//! tsvector's lexemes). A scan's conditions select the rows whose elements they need; every
//! candidate's conditions are checked on its row as before, so a posting list is only ever
//! a superset of the rows an operator holds for.

use super::{Placing, Posting, RowKeys, Shape};
use crate::types::json::Jsonb;
use crate::types::tsearch::{OP_AND, OP_NOT, OP_OR, OP_PHRASE, QueryItem};
use crate::types::{DataType, Value};
use std::collections::BTreeMap;
use std::ops::Bound;

/// An empty array, and an array with a NULL element (tokens of their own).
const EMPTY: u8 = b'0';
const NULLS: u8 = b'n';

/// Rows by element: `cols` are the index columns keyed (by `Shape::parts`).
pub(super) struct Elements {
    cols: Vec<usize>,
    fingerprint: String,
    pub(super) ts: u64,
    pub(super) rows: RowKeys,
    /// Token (index column, kind, bytes) to the rows having it, ascending.
    post: BTreeMap<Box<[u8]>, Posting>,
    /// Rows always read (a value it can't key).
    pub(super) always: Vec<u32>,
    built: usize,
}

fn token(col: usize, kind: u8, bytes: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(bytes.len() + 2);
    k.push(col as u8);
    k.push(kind);
    k.extend_from_slice(bytes);
    k
}

/// An array element's bytes, equal for elements equal under the element type's `=` (with a
/// deterministic collation: a condition under another isn't searched). None: not keyed.
fn elem_bytes(v: &Value, ty: &DataType) -> Option<Vec<u8>> {
    let tagged = |t: u8, b: &[u8]| {
        let mut k = vec![t];
        k.extend_from_slice(b);
        k
    };
    let float = |x: f64| {
        // (float8eq: NaN equals NaN, -0 equals 0.)
        let x = if x.is_nan() { f64::NAN } else if x == 0.0 { 0.0 } else { x };
        tagged(b'f', &x.to_bits().to_be_bytes())
    };
    Some(match v {
        Value::Int2(x) => tagged(b'i', &(*x as i64).to_be_bytes()),
        Value::Int4(x) => tagged(b'i', &(*x as i64).to_be_bytes()),
        Value::Int8(x) => tagged(b'i', &x.to_be_bytes()),
        Value::Float4(x) => float(*x as f64),
        Value::Float8(x) => float(*x),
        Value::Numeric(n) => tagged(b'#', n.normalize().to_string().as_bytes()),
        // (bpchar's `=` ignores trailing spaces: not keyed.)
        Value::Text(s) if matches!(ty, DataType::Text | DataType::Varchar(_) | DataType::Name) => tagged(b's', s.as_bytes()),
        Value::Bool(b) => tagged(b'b', &[*b as u8]),
        Value::Char(c) => tagged(b'c', &[*c]),
        Value::Date(d) => tagged(b'd', &d.to_be_bytes()),
        Value::Timestamp(x) | Value::Timestamptz(x) => tagged(b'T', &x.to_be_bytes()),
        Value::Uuid(u) => tagged(b'u', &u.to_be_bytes()),
        Value::Oid(o) => tagged(b'o', &o.to_be_bytes()),
        _ => return None,
    })
}

/// A jsonb value's tokens: every object key (`k`), and every scalar (`s` strings, `#`
/// numbers, `t` / `f`, `z` null) at any depth, a string array element (a raw scalar's too)
/// as `e`: jsonb_ops keys those as it keys object keys, its other strings apart. A value
/// containing another (`@>`) has every token of it; one with key or element `k` at its top
/// (`?`) has `k` or `e` of it.
fn json_tokens(col: usize, j: &Jsonb, out: &mut Vec<Vec<u8>>) {
    // (A raw scalar is a one-element array's.)
    if let Jsonb::String(s) = j {
        out.push(token(col, b'e', s.as_bytes()));
        return;
    }
    let mut stack = vec![j];
    while let Some(j) = stack.pop() {
        match j {
            Jsonb::Object(pairs) => {
                for (k, v) in pairs {
                    out.push(token(col, b'k', k.as_bytes()));
                    stack.push(v);
                }
            }
            Jsonb::Array(items) => {
                for it in items {
                    match it {
                        Jsonb::String(s) => out.push(token(col, b'e', s.as_bytes())),
                        it => stack.push(it),
                    }
                }
            }
            Jsonb::String(s) => out.push(token(col, b's', s.as_bytes())),
            Jsonb::Number(n) => out.push(token(col, b'#', n.normalize().to_string().as_bytes())),
            Jsonb::Bool(b) => out.push(token(col, if *b { b't' } else { b'f' }, &[])),
            Jsonb::Null => out.push(token(col, b'z', &[])),
        }
    }
}

/// A row's tokens for its value `v` of index column `col` (false: a value not keyed).
fn row_tokens(col: usize, v: &Value, out: &mut Vec<Vec<u8>>) -> bool {
    match v {
        // (No operator holds for NULL.)
        Value::Null => {}
        Value::Array(a) => {
            if a.items.is_empty() {
                out.push(token(col, EMPTY, &[]));
            }
            for e in &a.items {
                if e.is_null() {
                    out.push(token(col, NULLS, &[]));
                    continue;
                }
                let Some(b) = elem_bytes(e, &a.elem) else { return false };
                out.push(token(col, b'e', &b));
            }
        }
        Value::Jsonb(j) => json_tokens(col, j, out),
        Value::TsVector(tv) => out.extend(tv.lexemes.iter().map(|l| token(col, b'l', l.word.as_bytes()))),
        _ => return false,
    }
    true
}

impl Placing for Elements {
    const ROW_CAP: bool = true;
    fn make(shape: Shape, fingerprint: String, ts: u64) -> Elements {
        Elements { cols: shape.parts, fingerprint, ts, rows: RowKeys::default(), post: BTreeMap::new(), always: Vec::new(), built: 0 }
    }
    fn add(&mut self, suffix: &[u8], vals: Option<&[Value]>) {
        let row = self.rows.push(suffix);
        let mut toks = Vec::new();
        let keyed = vals.is_some_and(|vals| self.cols.iter().all(|&c| vals.get(c).is_some_and(|v| row_tokens(c, v, &mut toks))));
        if !keyed {
            self.always.push(row);
            return;
        }
        toks.sort_unstable();
        toks.dedup();
        for t in toks {
            match self.post.get_mut(&t[..]) {
                Some(l) => l.push(row),
                None => {
                    let mut l = Posting::default();
                    l.push(row);
                    self.post.insert(t.into_boxed_slice(), l);
                }
            }
        }
    }
    fn built(&mut self) {
        self.built = self.rows.len();
        self.rows.shrink();
        self.post.values_mut().for_each(|l| l.bytes.shrink_to_fit());
    }
    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    fn stale(&self) -> bool {
        self.rows.len() > 2 * self.built + 4096
    }
    fn heap_bytes(&self) -> usize {
        // (A B-tree entry: its key and list, a share of its node.)
        self.rows.heap_bytes()
            + self.post.iter().map(|(k, l)| k.len() + l.heap_bytes() + std::mem::size_of::<(Box<[u8]>, Posting)>() * 3 / 2).sum::<usize>()
            + self.always.capacity() * 4
    }
    fn full(&self) -> bool {
        self.rows.full()
    }
}

/// The rows a condition may hold for: every one, or these (ascending).
pub(super) enum Sel {
    All,
    Rows(Vec<u32>),
}

fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (a, b) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if a.len() * 16 < b.len() {
        return a.iter().copied().filter(|r| b.binary_search(r).is_ok()).collect();
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

impl Sel {
    pub(super) fn and(self, o: Sel) -> Sel {
        match (self, o) {
            (Sel::All, x) | (x, Sel::All) => x,
            (Sel::Rows(a), Sel::Rows(b)) => Sel::Rows(intersect(&a, &b)),
        }
    }
    fn or(self, o: Sel) -> Sel {
        match (self, o) {
            (Sel::Rows(mut a), Sel::Rows(b)) => {
                a.extend(b);
                a.sort_unstable();
                a.dedup();
                Sel::Rows(a)
            }
            _ => Sel::All,
        }
    }
}

impl Elements {
    fn list(&self, t: &[u8]) -> &Posting {
        self.post.get(t).unwrap_or(&super::NO_ROWS)
    }

    /// The rows having any of `toks`.
    fn any(&self, toks: &[Vec<u8>]) -> Sel {
        let mut rows: Vec<u32> = toks.iter().flat_map(|t| self.list(t).iter()).collect();
        rows.sort_unstable();
        rows.dedup();
        Sel::Rows(rows)
    }

    /// The rows having every one of `toks` (none: every row).
    fn every(&self, toks: &[Vec<u8>]) -> Sel {
        if toks.is_empty() {
            return Sel::All;
        }
        Sel::Rows(super::every(toks.iter().map(|t| self.list(t)).collect()))
    }

    /// The rows with lexeme `word` (`prefix`: one starting with it) in column `col`.
    fn lexeme(&self, col: usize, word: &str, prefix: bool) -> Sel {
        let t = token(col, b'l', word.as_bytes());
        if !prefix {
            return Sel::Rows(self.list(&t).iter().collect());
        }
        let end = crate::storage::keys::prefix_end(&t);
        let toks: Vec<Vec<u8>> = self.post.range::<[u8], _>((Bound::Included(&t[..]), Bound::Excluded(&end[..]))).map(|(k, _)| k.to_vec()).collect();
        self.any(&toks)
    }

    /// The rows a tsquery's node `i` (polish notation: the right operand next, the left
    /// `left` on) may hold for.
    fn tsquery(&self, col: usize, items: &[QueryItem], i: usize) -> Sel {
        match items.get(i) {
            Some(QueryItem::Val(o)) if !o.word.is_empty() => self.lexeme(col, &o.word, o.prefix),
            Some(QueryItem::Opr { oper, left, .. }) => {
                let r = || self.tsquery(col, items, i + 1);
                let l = || self.tsquery(col, items, i + *left as usize);
                match *oper {
                    OP_AND | OP_PHRASE => l().and(r()),
                    OP_OR => l().or(r()),
                    // (`!x` holds for a row without x: any row.)
                    OP_NOT => Sel::All,
                    _ => Sel::All,
                }
            }
            _ => Sel::All,
        }
    }

    /// The rows `col op arg` may hold for.
    pub(super) fn select(&self, col: usize, op: &str, arg: &Value) -> Sel {
        if !self.cols.contains(&col) {
            return Sel::All;
        }
        // `?` a key: the key of an object, or a string element (a raw scalar's too).
        let key = |s: &str| [token(col, b'k', s.as_bytes()), token(col, b'e', s.as_bytes())];
        let texts = |a: &crate::types::array::ArrayValue| -> Vec<std::sync::Arc<str>> {
            a.items.iter().filter_map(|v| if let Value::Text(s) = v { Some(s.clone()) } else { None }).collect()
        };
        match (op, arg) {
            ("&&" | "@>" | "<@" | "=", Value::Array(a)) => {
                let mut toks = Vec::new();
                let mut nulls = false;
                for e in &a.items {
                    if e.is_null() {
                        nulls = true;
                        continue;
                    }
                    match elem_bytes(e, &a.elem) {
                        Some(b) => toks.push(token(col, b'e', &b)),
                        // (A union needs every element.)
                        None if matches!(op, "&&" | "<@") => return Sel::All,
                        None => {}
                    }
                }
                match op {
                    "&&" => self.any(&toks),
                    "<@" => {
                        toks.push(token(col, EMPTY, &[]));
                        self.any(&toks)
                    }
                    "=" if a.items.is_empty() => Sel::Rows(self.list(&token(col, EMPTY, &[])).iter().collect()),
                    _ => {
                        // (array_eq: NULL elements equal each other.)
                        if nulls && op == "=" {
                            toks.push(token(col, NULLS, &[]));
                        }
                        self.every(&toks)
                    }
                }
            }
            ("@>", Value::Jsonb(j)) => {
                let mut toks = Vec::new();
                json_tokens(col, j, &mut toks);
                toks.sort_unstable();
                toks.dedup();
                self.every(&toks)
            }
            ("?", Value::Text(s)) => self.any(&key(s)),
            ("?|", Value::Array(a)) => self.any(&texts(a).iter().flat_map(|s| key(s)).collect::<Vec<_>>()),
            ("?&", Value::Array(a)) => texts(a).iter().fold(Sel::All, |acc, s| acc.and(self.any(&key(s)))),
            ("@@" | "@@@", Value::TsQuery(q)) => self.tsquery(col, &q.items, 0),
            _ => Sel::All,
        }
    }
}

/// The columns of index `bix` `Elements` keys: GIN ones whose operator class it knows.
pub(super) fn element_columns(bix: &crate::sql::plan::BoundIndex, ctx: &super::ExecCtx) -> Vec<usize> {
    if bix.index.method != "gin" {
        return Vec::new();
    }
    let cat = ctx.catalog();
    let mut cols = Vec::new();
    for (i, (c, k)) in bix.index.columns.iter().zip(&bix.keys).enumerate() {
        // (A built-in class, or one an extension made: intarray's.)
        let name = crate::catalog::system::opclass_name(c.opclass)
            .map(|(n, _)| n)
            .or_else(|| cat.opclasses.values().find(|o| o.oid == c.opclass).map(|o| o.name.as_str()));
        let known = match name.filter(|_| c.opclass != 0) {
            Some(n) => matches!(n, "array_ops" | "gin__int_ops" | "jsonb_ops" | "jsonb_path_ops" | "tsvector_ops"),
            // (The type's default: array_ops, jsonb_ops, tsvector_ops.)
            None => c.opclass == 0 && matches!(k.ty(), DataType::Array(_) | DataType::Jsonb | DataType::TsVector),
        };
        if known {
            cols.push(i);
        }
    }
    cols
}

/// A jsonpath item of a path GIN extracts conditions along (jsonb_gin.c's
/// JsonPathGinPathItem): a key, `.**`, `.*`, or an array accessor.
#[derive(Clone)]
enum PathItem {
    Key(String),
    Any,
    AnyKey,
    Array,
}

/// The GIN entries a jsonpath condition needs (JsonPathGinNode): the rows having one of
/// these tokens, those every node selects, those any node selects.
enum JpNode {
    Entry(Vec<Vec<u8>>),
    And(Vec<JpNode>),
    Or(Vec<JpNode>),
}

/// gin_extract_jsonb_query's jsonpath extraction (extract_jsp_query) for `@?` and `@@`,
/// under jsonb_ops or jsonb_path_ops (`path_ops`), over column `col`'s tokens. A
/// jsonb_ops key entry selects object keys and string array elements, both of which this
/// structure keys (`k`, `s`); jsonb_path_ops' entry, a hash of the path's keys and the
/// value, selects the rows having each of those keys and the value.
struct JpExtract {
    col: usize,
    path_ops: bool,
    lax: bool,
}

impl JpExtract {
    /// jsonb_ops__add_path_item / jsonb_path_ops__add_path_item (false: not supported).
    fn add(&self, path: &mut Vec<PathItem>, base: &crate::types::jsonpath::Node, acc: Option<&crate::types::jsonpath::Acc>) -> bool {
        use crate::types::jsonpath::{Acc, Node};
        match (base, acc) {
            (Node::Root, None) => path.clear(),
            (_, Some(Acc::Key(k))) => path.push(PathItem::Key(k.clone())),
            (_, Some(Acc::Index(_) | Acc::AnyArray)) => path.push(PathItem::Array),
            (_, Some(Acc::Any(..))) if !self.path_ops => path.push(PathItem::Any),
            (_, Some(Acc::AnyKey)) if !self.path_ops => path.push(PathItem::AnyKey),
            _ => return false,
        }
        true
    }

    /// A key entry for string `s`: an object key, or a string element.
    fn key(&self, s: &str) -> Vec<Vec<u8>> {
        vec![token(self.col, b'k', s.as_bytes()), token(self.col, b'e', s.as_bytes())]
    }

    /// jsonb_ops__extract_nodes / jsonb_path_ops__extract_nodes.
    fn extract_nodes(&self, path: &[PathItem], scalar: Option<&crate::types::jsonpath::Node>, nodes: &mut Vec<JpNode>) {
        use crate::types::jsonpath::Node;
        let Some(scalar) = scalar else { return };
        for it in path {
            if let PathItem::Key(k) = it {
                nodes.push(JpNode::Entry(if self.path_ops { vec![token(self.col, b'k', k.as_bytes())] } else { self.key(k) }));
            }
        }
        let col = self.col;
        nodes.push(JpNode::Entry(match scalar {
            Node::Null => vec![token(col, b'z', &[])],
            Node::Bool(b) => vec![token(col, if *b { b't' } else { b'f' }, &[])],
            Node::Num(x) => vec![token(col, b'#', x.normalize().to_string().as_bytes())],
            // (jsonb_path_ops: a value at the path, an element or not. jsonb_ops: a key
            // entry, a value's, or both (lax, or after `.**`), as the last item says.)
            Node::Str(s) if self.path_ops => vec![token(col, b's', s.as_bytes()), token(col, b'e', s.as_bytes())],
            Node::Str(s) => {
                let last = path.last();
                let key = !self.lax && matches!(last, Some(PathItem::Array));
                let value = !self.lax && !matches!(last, Some(PathItem::Array | PathItem::Any));
                let mut t = if value { Vec::new() } else { self.key(s) };
                if !key {
                    t.push(token(col, b's', s.as_bytes()));
                }
                t
            }
            _ => return,
        }));
    }

    /// extract_jsp_path_expr: `EXISTS(n)`, or `n == scalar`.
    fn path_expr(&self, mut path: Vec<PathItem>, n: &crate::types::jsonpath::Node, scalar: Option<&crate::types::jsonpath::Node>) -> Option<JpNode> {
        use crate::types::jsonpath::{Acc, Node};
        let mut nodes = Vec::new();
        let (base, accs): (&Node, &[Acc]) = match n {
            Node::Chain(b, a) => (b, a),
            n => (n, &[]),
        };
        let supported = (matches!(base, Node::Current) || self.add(&mut path, base, None))
            && accs.iter().all(|a| match a {
                Acc::Filter(f) => {
                    nodes.extend(self.bool_expr(path.clone(), f, false));
                    true
                }
                a => self.add(&mut path, base, Some(a)),
            });
        if supported {
            self.extract_nodes(&path, scalar, &mut nodes);
        }
        match nodes.len() {
            0 => None,
            1 => nodes.pop(),
            _ => Some(JpNode::And(nodes)),
        }
    }

    /// extract_jsp_bool_expr.
    fn bool_expr(&self, path: Vec<PathItem>, n: &crate::types::jsonpath::Node, not: bool) -> Option<JpNode> {
        use crate::types::jsonpath::{CmpOp, Node};
        crate::stack::grow(|| match n {
            Node::And(l, r) | Node::Or(l, r) => {
                let and = matches!(n, Node::And(..));
                match (self.bool_expr(path.clone(), l, not), self.bool_expr(path, r, not)) {
                    (Some(l), Some(r)) => Some(if not ^ and { JpNode::And(vec![l, r]) } else { JpNode::Or(vec![l, r]) }),
                    _ if !and => None,
                    (l, r) => l.or(r),
                }
            }
            Node::Not(e) => self.bool_expr(path, e, !not),
            Node::Exists(e) if !not => self.path_expr(path, e, None),
            Node::Cmp(CmpOp::Eq, l, r) if !not => {
                let scalar = |x: &Node| matches!(x, Node::Null | Node::Bool(_) | Node::Num(_) | Node::Str(_));
                let (p, s) = if scalar(l) {
                    (r, l)
                } else if scalar(r) {
                    (l, r)
                } else {
                    return None;
                };
                self.path_expr(path, p, Some(s))
            }
            _ => None,
        })
    }
}

impl Elements {
    fn jp_rows(&self, n: &JpNode) -> Sel {
        match n {
            JpNode::Entry(toks) => self.any(toks),
            JpNode::And(xs) => xs.iter().fold(Sel::All, |acc, x| acc.and(self.jp_rows(x))),
            JpNode::Or(xs) => xs.iter().map(|x| self.jp_rows(x)).reduce(Sel::or).unwrap_or(Sel::All),
        }
    }

    /// The rows jsonb column `col` `@?` (`exists`) or `@@` jsonpath `path` may hold for,
    /// as gin_extract_jsonb_query's entries for it select them (under jsonb_path_ops when
    /// `path_ops`): every row when it extracts none.
    pub(super) fn jsonpath(&self, col: usize, exists: bool, path_ops: bool, path: &str) -> Sel {
        if !self.cols.contains(&col) {
            return Sel::All;
        }
        let Ok(jp) = crate::types::jsonpath::parse(path) else { return Sel::All };
        let x = JpExtract { col, path_ops, lax: !jp.strict };
        let node = if exists { x.path_expr(Vec::new(), &jp.expr, None) } else { x.bool_expr(Vec::new(), &jp.expr, false) };
        node.map_or(Sel::All, |n| self.jp_rows(&n))
    }
}

/// Whether index `bix`'s column `col` has jsonb_path_ops.
pub(super) fn path_ops(bix: &crate::sql::plan::BoundIndex, col: usize) -> bool {
    bix.index.columns.get(col).and_then(|c| crate::catalog::system::opclass_name(c.opclass)).is_some_and(|(n, _)| n == "jsonb_path_ops")
}
