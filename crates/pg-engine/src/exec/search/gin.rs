//! GIN's search structure over arrays, jsonb and tsvector (`array_ops`, intarray's
//! `gin__int_ops`, `jsonb_ops`, `jsonb_path_ops`, `tsvector_ops`): a posting list of rows
//! per element, as GIN keys them (an array's elements, a jsonb value's keys and scalars, a
//! tsvector's lexemes). A scan's conditions select the rows whose elements they need; every
//! candidate's conditions are checked on its row as before, so a posting list is only ever
//! a superset of the rows an operator holds for (exactly those where `Elements::exact` says).

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
    /// Those of `cols` with jsonb_path_ops: keyed by their path tokens alone.
    path_ops: Vec<usize>,
    fingerprint: String,
    pub(super) ts: u64,
    pub(super) rows: RowKeys,
    /// Token (index column, kind, bytes) to the rows having it, ascending.
    post: BTreeMap<Box<[u8]>, Posting>,
    /// Rows always read (a value it can't key).
    pub(super) always: Vec<u32>,
    built: usize,
    /// A commit wrote the table since it was built (`Placing::written`): its postings may
    /// hold rows written again or deleted.
    pub(super) written: bool,
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
/// as `e`: jsonb_ops keys those as it keys object keys, its other strings apart (its jsonpath
/// entries select by them).
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

/// A jsonb value's path tokens (`P`): each scalar with the object keys above it, then how
/// many arrays it is in at each step (a raw scalar in none). A value contains another (`@>`)
/// only if it has every path token of it (keys are unique, a scalar is contained only by an
/// equal one at the same place, a raw scalar only by itself or a top-level array's element),
/// and does if besides every array element of the other gives one token: elements are
/// contained each by some element, and one token can't be split across two (`exact` false
/// otherwise, and for an empty object or array, which gives none).
struct Paths<'a> {
    col: usize,
    /// The keys above (`path_key`).
    keys: Vec<u8>,
    /// Arrays entered at each step: one more than the keys.
    arrs: Vec<u32>,
    out: &'a mut Vec<Vec<u8>>,
    exact: bool,
}

/// Key `k` as a step of a path token.
fn path_key(buf: &mut Vec<u8>, k: &str) {
    buf.push(1);
    buf.extend_from_slice(&(k.len() as u32).to_be_bytes());
    buf.extend_from_slice(k.as_bytes());
}

/// A path token's leaf after its keys: `s` a string, `#` a number (as `normalize` writes it:
/// equal under numeric's `=`, equal bytes), `t`, `f`, `z` null.
fn path_leaf(buf: &mut Vec<u8>, kind: u8, b: &[u8]) {
    buf.push(0);
    buf.push(kind);
    buf.extend_from_slice(&(b.len() as u32).to_be_bytes());
    buf.extend_from_slice(b);
}

fn json_leaf(j: &Jsonb) -> Option<(u8, std::borrow::Cow<'_, [u8]>)> {
    Some(match j {
        Jsonb::String(s) => (b's', s.as_bytes().into()),
        Jsonb::Number(n) => (b'#', n.normalize().to_string().into_bytes().into()),
        Jsonb::Bool(b) => (if *b { b't' } else { b'f' }, (&[][..]).into()),
        Jsonb::Null => (b'z', (&[][..]).into()),
        _ => return None,
    })
}

/// The tokens of `col`'s path tokens with these keys and this leaf, whatever arrays they are in.
fn path_prefix<'a>(col: usize, keys: impl Iterator<Item = &'a str>, kind: u8, b: &[u8]) -> Vec<u8> {
    let mut t = vec![col as u8, b'P'];
    keys.for_each(|k| path_key(&mut t, k));
    path_leaf(&mut t, kind, b);
    t
}

impl Paths<'_> {
    fn of(col: usize, j: &Jsonb, out: &mut Vec<Vec<u8>>) -> bool {
        let mut p = Paths { col, keys: Vec::new(), arrs: vec![0], out, exact: true };
        p.walk(j);
        p.exact
    }

    fn token(&self, kind: u8, b: &[u8], arrs: &[u32]) -> Vec<u8> {
        let mut t = Vec::with_capacity(self.keys.len() + b.len() + arrs.len() + 9);
        t.push(self.col as u8);
        t.push(b'P');
        t.extend_from_slice(&self.keys);
        path_leaf(&mut t, kind, b);
        for &a in arrs {
            let mut a = a;
            while a >= 0x80 {
                t.push(a as u8 | 0x80);
                a >>= 7;
            }
            t.push(a as u8);
        }
        t
    }

    /// The tokens `j` gives (pushed to `out`), how many.
    fn walk(&mut self, j: &Jsonb) -> usize {
        match j {
            Jsonb::Object(pairs) => {
                self.exact &= !pairs.is_empty();
                let mut n = 0;
                for (k, v) in pairs {
                    let mark = self.keys.len();
                    path_key(&mut self.keys, k);
                    self.arrs.push(0);
                    n += crate::stack::grow(|| self.walk(v));
                    self.arrs.pop();
                    self.keys.truncate(mark);
                }
                n
            }
            Jsonb::Array(items) => {
                self.exact &= !items.is_empty();
                *self.arrs.last_mut().unwrap() += 1;
                let mut n = 0;
                for it in items {
                    let m = crate::stack::grow(|| self.walk(it));
                    self.exact &= m == 1;
                    n += m;
                }
                *self.arrs.last_mut().unwrap() -= 1;
                n
            }
            j => {
                let (kind, b) = json_leaf(j).expect("a scalar");
                let t = self.token(kind, &b, &self.arrs);
                self.out.push(t);
                1
            }
        }
    }
}

/// `@>` of jsonb `q`'s tokens on column `col`: every one of the first, or (a raw scalar `q`)
/// any of them; whether the rows having them are exactly those containing `q`.
fn contained(col: usize, q: &Jsonb) -> (Vec<Vec<u8>>, bool, bool) {
    let mut toks = Vec::new();
    if let Some((kind, b)) = json_leaf(q) {
        // (A raw scalar contains it, and a top-level array with it as an element.)
        let p = Paths { col, keys: Vec::new(), arrs: Vec::new(), out: &mut Vec::new(), exact: true };
        return (vec![p.token(kind, &b, &[0]), p.token(kind, &b, &[1])], false, true);
    }
    let exact = Paths::of(col, q, &mut toks);
    toks.sort_unstable();
    toks.dedup();
    let exact = exact && !toks.is_empty();
    (toks, true, exact)
}

/// A row's tokens for its value `v` of index column `col` (false: a value not keyed),
/// `path_ops`: a jsonb value's path tokens alone.
fn row_tokens(col: usize, v: &Value, path_ops: bool, out: &mut Vec<Vec<u8>>) -> bool {
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
        Value::Jsonb(j) => {
            Paths::of(col, j, out);
            if path_ops {
                return true;
            }
            json_tokens(col, j, out);
            // (Its top level's keys and string elements apart (`K`, `E`), which `?`, `?|` and
            // `?&` test: an object's keys, an array's (a raw scalar's) strings.)
            match &**j {
                Jsonb::Object(pairs) => out.extend(pairs.iter().map(|(k, _)| token(col, b'K', k.as_bytes()))),
                Jsonb::Array(items) => out.extend(items.iter().filter_map(|it| match it {
                    Jsonb::String(s) => Some(token(col, b'E', s.as_bytes())),
                    _ => None,
                })),
                Jsonb::String(s) => out.push(token(col, b'E', s.as_bytes())),
                _ => {}
            }
        }
        Value::TsVector(tv) => out.extend(tv.lexemes.iter().map(|l| token(col, b'l', l.word.as_bytes()))),
        _ => return false,
    }
    true
}

impl Placing for Elements {
    const ROW_CAP: bool = true;
    fn make(shape: Shape, fingerprint: String, ts: u64) -> Elements {
        Elements { cols: shape.parts, path_ops: shape.path_ops, fingerprint, ts, rows: RowKeys::default(), post: BTreeMap::new(), always: Vec::new(), built: 0, written: false }
    }
    fn add(&mut self, suffix: &[u8], vals: Option<&[Value]>) {
        let row = self.rows.push(suffix);
        let mut toks = Vec::new();
        let keyed = vals.is_some_and(|vals| self.cols.iter().all(|&c| vals.get(c).is_some_and(|v| row_tokens(c, v, self.path_ops.contains(&c), &mut toks))));
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
    fn written(&mut self) {
        self.written = true;
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

    /// The rows having a token starting with `t`.
    fn prefixed(&self, t: &[u8]) -> Sel {
        let end = crate::storage::keys::prefix_end(t);
        let mut rows: Vec<u32> = self.post.range::<[u8], _>((Bound::Included(t), Bound::Excluded(&end[..]))).flat_map(|(_, l)| l.iter()).collect();
        rows.sort_unstable();
        rows.dedup();
        Sel::Rows(rows)
    }

    /// The rows with lexeme `word` (`prefix`: one starting with it) in column `col`.
    fn lexeme(&self, col: usize, word: &str, prefix: bool) -> Sel {
        let t = token(col, b'l', word.as_bytes());
        if !prefix {
            return Sel::Rows(self.list(&t).iter().collect());
        }
        self.prefixed(&t)
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

    /// Whether `select(col, op, arg)` is exactly the rows of those read that `col op arg`
    /// holds for, not more: a tsquery of AND and OR over lexemes without weights (a row has a
    /// lexeme, or one it prefixes), an array's `@>` or `&&` of elements it keys, none null
    /// (elements equal under `=` have equal bytes: `elem_bytes`), jsonb's `?`, `?|`, `?&` of
    /// strings (its top level's tokens), jsonb's `@>` of a value whose arrays' elements give
    /// one path token each (`Paths`). A row it couldn't key (`always`) makes none exact.
    pub(super) fn exact(&self, col: usize, op: &str, arg: &Value) -> bool {
        if !self.cols.contains(&col) || !self.always.is_empty() {
            return false;
        }
        match (op, arg) {
            ("@>", Value::Jsonb(j)) => contained(col, j).2,
            _ if self.path_ops.contains(&col) => false,
            ("@@" | "@@@", Value::TsQuery(q)) => {
                !q.items.is_empty()
                    && q.items.iter().all(|it| match it {
                        QueryItem::Val(o) => o.weight == 0 && !o.word.is_empty(),
                        QueryItem::Opr { oper, .. } => matches!(*oper, OP_AND | OP_OR),
                        QueryItem::Stop => false,
                    })
            }
            ("@>" | "&&", Value::Array(a)) => {
                !a.items.is_empty() && a.items.iter().all(|e| !e.is_null() && elem_bytes(e, &a.elem).is_some())
            }
            ("?", Value::Text(_)) => true,
            ("?|" | "?&", Value::Array(a)) => !a.items.is_empty() && a.items.iter().all(|v| matches!(v, Value::Text(_))),
            _ => false,
        }
    }

    /// The rows `col op arg` may hold for.
    pub(super) fn select(&self, col: usize, op: &str, arg: &Value) -> Sel {
        if !self.cols.contains(&col) {
            return Sel::All;
        }
        if let ("@>", Value::Jsonb(j)) = (op, arg) {
            return match contained(col, j) {
                (toks, false, _) => self.any(&toks),
                (toks, true, _) => self.every(&toks),
            };
        }
        // (jsonb_path_ops keys paths alone, which its other operators don't search.)
        if self.path_ops.contains(&col) {
            return Sel::All;
        }
        // `?` a key: the key of the top-level object, or a string element of the top-level
        // array (a raw scalar's too).
        let key = |s: &str| [token(col, b'K', s.as_bytes()), token(col, b'E', s.as_bytes())];
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
    /// The tokens starting with it.
    Prefix(Vec<u8>),
    And(Vec<JpNode>),
    Or(Vec<JpNode>),
}

/// gin_extract_jsonb_query's jsonpath extraction (extract_jsp_query) for `@?` and `@@`,
/// under jsonb_ops or jsonb_path_ops (`path_ops`), over column `col`'s tokens. A
/// jsonb_ops key entry selects object keys and string array elements, both of which this
/// structure keys (`k`, `s`); jsonb_path_ops' entry, a hash of the path's keys and the
/// value, selects the rows with the value under those keys (`Paths`' tokens of any arrays).
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
        if self.path_ops {
            let keys = path.iter().filter_map(|it| if let PathItem::Key(k) = it { Some(k.as_str()) } else { None });
            let (kind, b): (u8, std::borrow::Cow<'_, [u8]>) = match scalar {
                Node::Null => (b'z', (&[][..]).into()),
                Node::Bool(b) => (if *b { b't' } else { b'f' }, (&[][..]).into()),
                Node::Num(x) => (b'#', x.normalize().to_string().into_bytes().into()),
                Node::Str(s) => (b's', s.as_bytes().into()),
                _ => return,
            };
            nodes.push(JpNode::Prefix(path_prefix(self.col, keys, kind, &b)));
            return;
        }
        for it in path {
            if let PathItem::Key(k) = it {
                nodes.push(JpNode::Entry(self.key(k)));
            }
        }
        let col = self.col;
        nodes.push(JpNode::Entry(match scalar {
            Node::Null => vec![token(col, b'z', &[])],
            Node::Bool(b) => vec![token(col, if *b { b't' } else { b'f' }, &[])],
            Node::Num(x) => vec![token(col, b'#', x.normalize().to_string().as_bytes())],
            // (A key entry, a value's, or both (lax, or after `.**`), as the last item says.)
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
            JpNode::Prefix(t) => self.prefixed(t),
            JpNode::And(xs) => xs.iter().fold(Sel::All, |acc, x| acc.and(self.jp_rows(x))),
            JpNode::Or(xs) => xs.iter().map(|x| self.jp_rows(x)).reduce(Sel::or).unwrap_or(Sel::All),
        }
    }

    /// The rows jsonb column `col` `@?` (`exists`) or `@@` jsonpath `path` may hold for,
    /// as gin_extract_jsonb_query's entries for it select them (under the column's class):
    /// every row when it extracts none.
    pub(super) fn jsonpath(&self, col: usize, exists: bool, path: &str) -> Sel {
        if !self.cols.contains(&col) {
            return Sel::All;
        }
        let Ok(jp) = crate::types::jsonpath::parse(path) else { return Sel::All };
        let x = JpExtract { col, path_ops: self.path_ops.contains(&col), lax: !jp.strict };
        let node = if exists { x.path_expr(Vec::new(), &jp.expr, None) } else { x.bool_expr(Vec::new(), &jp.expr, false) };
        node.map_or(Sel::All, |n| self.jp_rows(&n))
    }
}

/// Whether index `bix`'s column `col` has jsonb_path_ops.
pub(super) fn path_ops(bix: &crate::sql::plan::BoundIndex, col: usize) -> bool {
    bix.index.columns.get(col).and_then(|c| crate::catalog::system::opclass_name(c.opclass)).is_some_and(|(n, _)| n == "jsonb_path_ops")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonb_selection_matches_evaluator() {
        use crate::types::json::parse_jsonb;
        use crate::types::jsonpath;

        // The same behavioral corpus is recorded against PG18.6 by the SQL golden.
        let sql = include_str!("../../../tests/sql/a192_gin_jsonb_paths.sql");
        let mut docs = parse_jsonb(sql.split("$matrix$").nth(1).unwrap()).unwrap().into_array().unwrap();
        let tz = crate::types::datetime::TimeZone::gmt();
        let base = docs.clone();
        for j in base {
            docs.push(parse_jsonb(&format!("[{}]", j)).unwrap());
            docs.push(parse_jsonb(&format!("{{\"a\":{}}}", j)).unwrap());
        }
        for path_ops in [false, true] {
            let sh = Shape { dim: None, parts: vec![0], tys: Vec::new(), path_ops: if path_ops { vec![0] } else { Vec::new() } };
            let mut el = Elements::make(sh, String::new(), 0);
            for (i, j) in docs.iter().enumerate() {
                el.add(&(i as u64).to_be_bytes(), Some(&[Value::Jsonb(j.clone())]));
            }
            el.add(&(docs.len() as u64).to_be_bytes(), Some(&[Value::Null]));
            let selected = |s: &Sel, i: usize| match s {
                Sel::All => true,
                Sel::Rows(rows) => rows.binary_search(&(i as u32)).is_ok(),
            };
            let mut exact_queries = 0;
            for q in &docs {
                let arg = Value::Jsonb(q.clone());
                let sel = el.select(0, "@>", &arg);
                let exact = el.exact(0, "@>", &arg);
                exact_queries += usize::from(exact);
                for (i, j) in docs.iter().enumerate() {
                    let holds = j.contains(q);
                    let hit = selected(&sel, i);
                    assert!(!holds || hit, "false negative: path_ops={path_ops}, {j} @> {q}");
                    if exact {
                        assert_eq!(hit, holds, "false exact: path_ops={path_ops}, {j} @> {q}");
                    }
                }
                if exact {
                    assert!(!selected(&sel, docs.len()), "SQL NULL cannot contain {q}");
                }
            }
            assert!(exact_queries > 20, "exercise exact selections");
            for path in ["$.a == 1", "$.a.b == 1", "$.a[*].b == 1", "$.a == null", "$.a == \"x\"",
                "$.a.b == 1 && $.a.c == 2", "$.a.b == 1 || $.ab == 1", "$.** == 1", "$[*] == 1", "$ == \"x\""] {
                for mode in ["lax", "strict"] {
                    let path = format!("{mode} {path}");
                    let parsed = jsonpath::parse(&path).unwrap();
                    let sel = el.jsonpath(0, false, &path);
                    for (i, j) in docs.iter().enumerate() {
                        let holds = jsonpath::matches(j, &parsed, None, true, false, &tz).unwrap() == Some(true);
                        assert!(!holds || selected(&sel, i), "jsonpath false negative: path_ops={path_ops}, {j} @@ {path}");
                    }
                }
            }
            // Container and multi-leaf array-element queries retain ordinary recheck.
            for q in ["{}", "[]", r#"[{"a":1,"b":2}]"#, r#"{"a":[{"b":1},{}]}"#] {
                assert!(!el.exact(0, "@>", &Value::Jsonb(parse_jsonb(q).unwrap())));
            }
            assert!(!el.exact(0, "@@", &Value::Text("$.a == 1".into())));
        }
    }
}
