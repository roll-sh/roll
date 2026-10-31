//! Statement templates: a query's text with its literals taken out.
//!
//! Applications send the same statements again and again with other constants (`UPDATE t SET
//! v = v + 17 WHERE id = 4711`), so a parse tree cached by text rarely hits. `scan` finds a
//! query's numeric and string literals with Postgres's scanner rules and makes a key of the
//! rest, each literal's bytes replaced by its class (integer, bigint-sized integer, numeric,
//! string) so the key keeps every other byte where it was. Two queries with one key scan into
//! the same tokens but for those literals' values, so the grammar builds the same tree but for
//! the A_Const nodes the literals make: `parse` keeps one tree per key and makes another
//! query's by putting its literals' values into those nodes (locations are the same, as every
//! literal keeps its length). A query whose literals can't all be found as A_Consts of its
//! tree (a literal the grammar keeps as a plain number or string) isn't templated.

use super::nodewalk;
use crate::error::Result;
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, RawStmt, a_const::Val};
use std::cell::RefCell;
use std::sync::Arc;

/// What a literal makes: `Int4` an integer (ICONST), `Int8` / `Numeric` a Float node (an
/// integer too big for int4 that fits in int8; a decimal or a bigger integer), `Str` a string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LitKind {
    Int4,
    Int8,
    Numeric,
    Str,
}

impl LitKind {
    /// The byte the key has in place of each of the literal's bytes.
    fn mark(self) -> u8 {
        match self {
            LitKind::Int4 => 1,
            LitKind::Int8 => 2,
            LitKind::Numeric => 3,
            LitKind::Str => 4,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Lit {
    pub start: usize,
    pub end: usize,
    pub kind: LitKind,
}

#[derive(Debug)]
pub struct Scan {
    pub key: Box<str>,
    pub lits: Vec<Lit>,
}

/// Longer queries aren't templated.
const MAX_TEXT: usize = 4096;

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Past a `--` comment starting at `i`.
fn line_comment_end(s: &[u8], i: usize) -> usize {
    s[i..].iter().position(|&b| b == b'\n' || b == b'\r').map_or(s.len(), |p| i + p)
}

/// Past a (nested) `/* */` comment starting at `i`; None unterminated.
fn block_comment_end(s: &[u8], mut i: usize) -> Option<usize> {
    let mut depth = 0;
    while i + 1 < s.len() {
        match (s[i], s[i + 1]) {
            (b'/', b'*') => {
                depth += 1;
                i += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Whether the next token from `i` (past whitespace and comments) starts with a quote: a
/// string continued over a newline, or a syntax error; neither is templated.
fn quote_follows(s: &[u8], mut i: usize) -> bool {
    loop {
        match s.get(i) {
            Some(b) if b.is_ascii_whitespace() => i += 1,
            Some(b'-') if s.get(i + 1) == Some(&b'-') => i = line_comment_end(s, i),
            Some(b'/') if s.get(i + 1) == Some(&b'*') => match block_comment_end(s, i) {
                Some(e) => i = e,
                None => return false,
            },
            Some(b'\'') => return true,
            _ => return false,
        }
    }
}

/// The query's template key and literals, for a query of one statement read with the
/// default scanner settings (`standard_conforming_strings` on); None when it has none or
/// anything the template doesn't handle (dollar quotes, escape / bit / Unicode strings,
/// exponents, non-decimal or underscored numbers, several statements).
pub fn scan(sql: &str) -> Option<Scan> {
    let s = sql.as_bytes();
    if s.len() > MAX_TEXT {
        return None;
    }
    let mut key = Vec::with_capacity(s.len());
    let mut lits = Vec::new();
    let mut ended = false;
    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        let start = i;
        if ended && !b.is_ascii_whitespace() {
            return None;
        }
        match b {
            b'-' if s.get(i + 1) == Some(&b'-') => i = line_comment_end(s, i),
            b'/' if s.get(i + 1) == Some(&b'*') => i = block_comment_end(s, i)?,
            b'\'' => {
                if i > 0 && (ident_byte(s[i - 1]) || s[i - 1] == b'&') {
                    return None;
                }
                i += 1;
                loop {
                    match s.get(i)? {
                        b'\'' if s.get(i + 1) == Some(&b'\'') => i += 2,
                        b'\'' => break,
                        _ => i += 1,
                    }
                }
                i += 1;
                if quote_follows(s, i) {
                    return None;
                }
                lits.push(Lit { start, end: i, kind: LitKind::Str });
                key.resize(key.len() + (i - start), LitKind::Str.mark());
                continue;
            }
            b'"' => {
                i += 1;
                loop {
                    match s.get(i)? {
                        b'"' if s.get(i + 1) == Some(&b'"') => i += 2,
                        b'"' => break,
                        _ => i += 1,
                    }
                }
                i += 1;
            }
            b'$' => {
                // A parameter; anything else here starts a dollar quote.
                if !s.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    return None;
                }
                i += 1;
                while s.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
            }
            b'0'..=b'9' => {
                while s.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                let mut kind = LitKind::Int4;
                if s.get(i) == Some(&b'.') {
                    if !s.get(i + 1).is_some_and(u8::is_ascii_digit) {
                        return None;
                    }
                    i += 1;
                    while s.get(i).is_some_and(u8::is_ascii_digit) {
                        i += 1;
                    }
                    kind = LitKind::Numeric;
                }
                // (An exponent, `0x`, `1_000`, trailing junk: not templated.)
                if s.get(i).is_some_and(|&c| ident_byte(c) || c == b'.') {
                    return None;
                }
                if kind == LitKind::Int4 && sql[start..i].parse::<i32>().is_err() {
                    kind = if sql[start..i].parse::<i64>().is_ok() { LitKind::Int8 } else { LitKind::Numeric };
                }
                lits.push(Lit { start, end: i, kind });
                key.resize(key.len() + (i - start), kind.mark());
                continue;
            }
            b'.' if s.get(i + 1).is_some_and(u8::is_ascii_digit) => return None,
            b';' => {
                ended = true;
                i += 1;
            }
            _ if ident_byte(b) => {
                while s.get(i).is_some_and(|&c| ident_byte(c)) {
                    i += 1;
                }
            }
            _ => i += 1,
        }
        key.extend_from_slice(&s[start..i]);
    }
    if lits.is_empty() {
        return None;
    }
    Some(Scan { key: String::from_utf8(key).ok()?.into_boxed_str(), lits })
}

/// The value literal `l` of `sql` gives its A_Const (negated by unary minus signs or not).
pub fn lit_val(sql: &str, l: &Lit, neg: bool) -> Val {
    let text = &sql[l.start..l.end];
    match l.kind {
        LitKind::Int4 => {
            let v: i32 = text.parse().unwrap_or(0);
            Val::Ival(pb::Integer { ival: if neg { -v } else { v } })
        }
        LitKind::Int8 | LitKind::Numeric => Val::Fval(pb::Float {
            fval: if neg { format!("-{text}") } else { text.to_string() },
        }),
        LitKind::Str => Val::Sval(pb::String {
            sval: text[1..text.len() - 1].replace("''", "'"),
        }),
    }
}

/// A key's tree, and for each A_Const in `nodewalk` order the literal it holds (with
/// whether it is negated), None for one no literal made (NULL, TRUE, ...).
pub struct Template {
    pub stmts: Vec<RawStmt>,
    pub consts: Vec<Option<(u32, bool)>>,
}

/// The A_Consts of `stmts` (parsed from `sql`) matched with `sc`'s literals: an A_Const of
/// a literal is where the literal starts, or (doNegate) at the first of the unary minus
/// signs before it, its value negated once per sign. None unless every literal has one.
fn build(sql: &str, sc: &Scan, stmts: &[RawStmt]) -> Option<Template> {
    let s = sql.as_bytes();
    let mut t = Template { stmts: stmts.to_vec(), consts: Vec::new() };
    let mut seen = vec![false; sc.lits.len()];
    let mut ok = true;
    for st in &mut t.stmts {
        nodewalk::raw_stmt(st, &mut |n| {
            let Some(NodeEnum::AConst(c)) = &n.node else { return };
            let mut at = c.location.max(0) as usize;
            let mut minus = 0;
            while at < s.len() && (s[at] == b'-' || s[at].is_ascii_whitespace()) {
                minus += (s[at] == b'-') as usize;
                at += 1;
            }
            let slot = match sc.lits.binary_search_by_key(&at, |l| l.start) {
                Ok(k) if c.location >= 0 => {
                    let neg = minus % 2 == 1;
                    if c.isnull || c.val.as_ref() != Some(&lit_val(sql, &sc.lits[k], neg)) {
                        ok = false;
                    }
                    seen[k] = true;
                    Some((k as u32, neg))
                }
                _ => None,
            };
            t.consts.push(slot);
        });
    }
    (ok && seen.iter().all(|&x| x)).then_some(t)
}

/// `t`'s tree with `sql`'s literals (`sc`, scanned with `t`'s key) in its A_Consts.
pub fn instantiate(t: &Template, sql: &str, sc: &Scan) -> Vec<RawStmt> {
    let mut stmts = t.stmts.clone();
    let mut j = 0;
    for st in &mut stmts {
        nodewalk::raw_stmt(st, &mut |n| {
            let Some(NodeEnum::AConst(c)) = &mut n.node else { return };
            if let Some((k, neg)) = t.consts[j] {
                c.val = Some(lit_val(sql, &sc.lits[k as usize], neg));
            }
            j += 1;
        });
    }
    stmts
}

/// A key's template, None when its query can't be templated; with its text's length (whoever
/// drops it last drops it with the stack its depth needs).
struct Entry(Option<Arc<Template>>, usize);

impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(t) = self.0.take() {
            crate::stack::drop_deep(t, self.1);
        }
    }
}

/// Templates by key, shared by every session (a template is a pure function of its key).
static TEMPLATES: crate::lru::Shared<Box<str>, Arc<Entry>> =
    crate::lru::Shared::new("statement templates", 16 << 20);

/// The template cache's footprint (`lru::footprints`).
pub(crate) fn templates_footprint() -> (&'static str, crate::lru::Footprint) {
    TEMPLATES.footprint()
}

thread_local! {
    /// The last query `parse` read through a template, with it (`take_last`).
    static LAST: RefCell<Option<(Scan, Arc<Template>)>> = const { RefCell::new(None) };
}

/// The scan and template of the query this thread last parsed through one (and forget it).
pub fn take_last() -> Option<(Scan, Arc<Template>)> {
    LAST.with(|l| l.borrow_mut().take())
}

/// The template of `sc`'s key: made from `parse(sql)` the first time (None when the query
/// can't be templated, or failed to parse: then `parse`'s result is returned instead).
fn template(
    sql: &str,
    sc: &Scan,
    parse: &dyn Fn() -> Result<Vec<RawStmt>>,
) -> std::result::Result<Option<Arc<Template>>, Result<Vec<RawStmt>>> {
    if let Some(e) = TEMPLATES.get(&sc.key) {
        return Ok(e.0.clone());
    }
    let stmts = match parse() {
        Ok(s) => s,
        Err(e) => return Err(Err(e)),
    };
    let headroom = crate::stack::tree_headroom(sql.len());
    let t = crate::stack::with_headroom(headroom, || build(sql, sc, &stmts)).map(Arc::new);
    // (Its tree's bytes as the parse tree cache counts them, `parse::tree_bytes`.)
    let bytes = crate::stack::with_headroom(headroom, || super::parse::stmts_bytes(&stmts)) + 256;
    let e = Arc::new(Entry(t.clone(), sql.len()));
    drop(TEMPLATES.insert(sc.key.clone(), e, bytes));
    match t {
        Some(t) => Ok(Some(t)),
        None => Err(Ok(stmts)),
    }
}

/// `sql` parsed through its template (`parse` parses it when there is none).
pub fn parse(sql: &str, sc: Scan, parse: impl Fn() -> Result<Vec<RawStmt>>) -> Result<Vec<RawStmt>> {
    match template(sql, &sc, &parse) {
        Ok(Some(t)) => {
            let headroom = crate::stack::tree_headroom(sql.len());
            let stmts = crate::stack::with_headroom(headroom, || instantiate(&t, sql, &sc));
            LAST.with(|l| *l.borrow_mut() = Some((sc, t)));
            Ok(stmts)
        }
        Ok(None) => parse(),
        Err(r) => r,
    }
}

/// What a plan read when it was made besides its catalog and settings (`recording`): the
/// relation sizes the planner estimates from, as the size model gave them. A plan kept for a
/// template (`session::litplan`) is only used again while each reads the same; `Data` (a
/// column's current minimum or maximum, read from its rows) never does.
#[derive(Clone, Debug, PartialEq)]
pub enum SizeRead {
    /// A table's heap blocks (`costs::rel_size`).
    Heap(u32, u32),
    /// An index's blocks (`paths::index_infos`).
    Fork(u32, u64),
    /// A btree's height: (its table, index, the pages it was asked for, key width, height).
    BtreeLevel(u32, u32, u64, u32, u32),
    Data,
}

thread_local! {
    static READS: RefCell<Option<Vec<SizeRead>>> = const { RefCell::new(None) };
}

/// Note what the planner read, while `recording`.
pub fn note_read(r: impl FnOnce() -> SizeRead) {
    READS.with(|x| {
        if let Some(v) = x.borrow_mut().as_mut() {
            v.push(r());
        }
    });
}

/// `f`, and what it read (`note_read`).
pub fn recording<T>(f: impl FnOnce() -> T) -> (T, Vec<SizeRead>) {
    let outer = READS.with(|x| x.borrow_mut().replace(Vec::new()));
    let r = f();
    let reads = READS.with(|x| std::mem::replace(&mut *x.borrow_mut(), outer)).unwrap_or_default();
    (r, reads)
}

impl SizeRead {
    /// Whether reading it again gives what it gave.
    pub fn holds(&self, ctx: &crate::exec::ExecCtx) -> bool {
        use crate::exec::sizing;
        match self {
            SizeRead::Heap(oid, n) => ctx.catalog().table(*oid).is_some_and(|t| {
                sizing::heap_blocks(t, ctx).ok().flatten().unwrap_or(0) == *n
            }),
            SizeRead::Fork(oid, n) => {
                sizing::fork_blocks(*oid, sizing::Fork::Main, ctx).ok().flatten().unwrap_or(1) == *n
            }
            SizeRead::BtreeLevel(t, ix, pages, width, level) => {
                sizing::btree_level(*t, *ix, f64::from_bits(*pages), *width, &ctx.sess.db) == *level
            }
            SizeRead::Data => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(sql: &str) -> Option<Box<str>> {
        scan(sql).map(|s| s.key)
    }

    #[test]
    fn keys_leave_out_literals() {
        let k = key("UPDATE t SET a = a + 5 WHERE id = 12;");
        assert!(k.is_some());
        assert_eq!(k, key("UPDATE t SET a = a + 7 WHERE id = 99;"));
        assert_ne!(k, key("UPDATE t SET a = a + 10 WHERE id = 99;"));
        assert_ne!(key("SELECT 2147483647"), key("SELECT 2147483648"));
        assert_ne!(key("SELECT 1.5"), key("SELECT 125"));
        assert_eq!(key("SELECT 'ab' -- 12\n"), key("SELECT 'cd' -- 12\n"));
        assert_ne!(key("SELECT 'ab' -- 12\n"), key("SELECT 'ab' -- 13\n"));
        assert_eq!(key("SELECT t1.c2 FROM t1 WHERE c2 = $1"), None);
        for q in [
            "SELECT E'x'",
            "SELECT U&'x'",
            "SELECT B'101'",
            "SELECT $$x$$",
            "SELECT 1e5",
            "SELECT 0x1F",
            "SELECT 1_000",
            "SELECT 1.",
            "SELECT .5",
            "SELECT 12abc",
            "SELECT 1; SELECT 2",
            "SELECT 'a'\n'b'",
            "SELECT 'a",
        ] {
            assert!(scan(q).is_none(), "{q}");
        }
    }

    /// Each group's queries share a key; each parses through the template to what the
    /// parser makes of it (debug builds compare every templated parse with the parser's).
    #[test]
    fn templates_parse_as_the_parser_does() {
        let groups: &[&[&str]] = &[
            &["UPDATE t SET a = a + -5000 WHERE id = 12", "UPDATE t SET a = a + -4999 WHERE id = 98"],
            &["SELECT - -5, -0, 3 - -2", "SELECT - -7, -1, 8 - -9"],
            &["SELECT 3000000000, -3000000000", "SELECT 9000000000, -9000000000"],
            &["SELECT 99999999999999999999.5", "SELECT 12345678901234567890.1"],
            &["SELECT 'it''s', 'x' || 'y'", "SELECT 'ab''c', 'z' || 'q'"],
            &["SELECT date '2020-01-01', 1::numeric(10,2)", "SELECT date '2021-12-31', 2::numeric(12,4)"],
            &["SELECT * FROM t WHERE a IN (1, 2) AND b BETWEEN 3 AND 4 LIMIT 5", "SELECT * FROM t WHERE a IN (6, 7) AND b BETWEEN 8 AND 9 LIMIT 1"],
            &["INSERT INTO t (a, b) VALUES (1, 'x'), (-2, 'y')", "INSERT INTO t (a, b) VALUES (3, 'z'), (-4, 'w')"],
            &["SELECT \"c1\" /* 1 /* 2 */ */ FROM t1 WHERE c2 = 7 ORDER BY 1", "SELECT \"c1\" /* 1 /* 2 */ */ FROM t1 WHERE c2 = 8 ORDER BY 2"],
            &["SELECT 'é', 1", "SELECT 'ü', 2"],
        ];
        for g in groups {
            let keys: Vec<_> = g.iter().map(|q| key(q)).collect();
            assert!(keys[0].is_some() && keys.iter().all(|k| *k == keys[0]), "{g:?}");
            for q in *g {
                let (_, trees) = crate::sql::parse::parse_live(q).unwrap();
                assert_eq!(Some(&trees), crate::sql::parse::parse(q).ok().as_ref(), "{q}");
            }
        }
    }
}
