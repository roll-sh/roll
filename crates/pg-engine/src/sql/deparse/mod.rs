//! Parse trees back to SQL text: a port of libpg_query's deparser (`postgres_deparse.c`, as
//! `pg_query::deparse` runs it: not pretty-printed), byte for byte.
//!
//! Stored definitions (views, rules, defaults, checks, index expressions, policies, function
//! bodies, ...) keep the text `pg_query::deparse` made for them and parse it again later, so
//! this must give exactly that text, oddities included. `pg_query::deparse` encodes the tree
//! as protobuf first, and prost measures each nested message's subtree again at every level:
//! quadratic in the depth. This writes the text in one pass.
//!
//! The C deparser writes into "parts" (in pretty mode, lines), grouped under keywords and
//! nested in levels, and joins them at the end; even unpretty, parts merge up to 80 columns
//! and the joins drop and add spaces by rules of their own. `D` keeps that model: parts in
//! one list in the order they were written, a level's groups and parts a suffix of it.
//!
//! Anything the port doesn't cover returns `Unhandled`, and the caller falls back to
//! `pg_query::deparse` for the whole statement: so do the C deparser's error paths and its
//! `Assert(false)` cases.

mod expr;
mod from;
mod func;
mod json;
mod stmt;

use pg_query::NodeEnum;
use pg_query::protobuf::Node;
use std::sync::OnceLock;

/// A tree this port doesn't deparse: libpg_query's deparser has to.
#[derive(Debug)]
pub struct Unhandled;

type R = std::result::Result<(), Unhandled>;

/// A statement's text, as `pg_query::deparse` gives it.
pub fn statement(n: &Node) -> Result<String, Unhandled> {
    let mut d = D::default();
    d.stmt(n)?;
    Ok(d.finish())
}

/// An expression's text, as `pg_query::deparse` gives it for `SELECT <expr>`, the `SELECT `
/// stripped.
pub fn expression(n: &Node) -> Result<String, Unhandled> {
    let mut d = D::default();
    d.select_one(n)?;
    let sql = d.finish();
    Ok(match sql.strip_prefix("SELECT ") {
        Some(s) => s.to_string(),
        None => sql,
    })
}

/// Every expression node the deparser visits in a statement, each with its own text as
/// `expression` gives it: for the identity test (`tests/deparse_identity.rs`).
#[doc(hidden)]
pub fn expressions(n: &Node) -> Result<Vec<&Node>, Unhandled> {
    let mut d = D {
        seen: Some(Vec::new()),
        ..D::default()
    };
    d.stmt(n)?;
    // SAFETY: the pointers are to nodes inside `n`, which outlives the result.
    Ok(d.seen.unwrap().into_iter().map(|p| unsafe { &*p }).collect())
}

/// How a group indents its parts (`DeparsePartIndentMode`); only merging matters unpretty.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    NoIndent,
    Indent,
    Merge,
}

/// The node context of the C deparser that changes how a node prints.
#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    None,
    AExpr,
    BExpr,
    FuncExpr,
    InsertSelect,
    SetOp,
    SortClause,
    InsertRelation,
}

struct Part {
    text: String,
    indent: usize,
    mergeable: bool,
}

#[derive(Clone, Copy)]
struct Group {
    /// The group starts with its keyword's part.
    keyword: bool,
    mode: Mode,
    /// Its first part (the keyword's).
    first: usize,
}

struct Level {
    base: usize,
    first_group: usize,
}

/// Merged parts stay within this many columns (libpg_query's default `max_line_length`).
const MAX_LINE: usize = 80;
/// libpg_query's default `indent_size`.
const INDENT: usize = 4;

#[derive(Default)]
struct D {
    parts: Vec<Part>,
    groups: Vec<Group>,
    levels: Vec<Level>,
    /// The expression nodes deparsed, when collecting them (`expressions`).
    seen: Option<Vec<*const Node>>,
}

fn strip1(s: &mut String) {
    if s.ends_with(' ') {
        s.pop();
    }
}

impl D {
    fn base(&self) -> usize {
        self.levels.last().map_or(0, |l| l.base)
    }

    fn new_part(&self, mode: Mode) -> Part {
        Part {
            text: String::new(),
            indent: self.base() + if mode == Mode::NoIndent { 0 } else { INDENT },
            mergeable: mode == Mode::Merge,
        }
    }

    /// The current level's last group, started (with an empty part) if it has none.
    fn cur_group(&mut self) -> usize {
        let first = self.levels.last().map_or(0, |l| l.first_group);
        if first == self.groups.len() {
            self.groups.push(Group {
                keyword: false,
                mode: Mode::NoIndent,
                first: self.parts.len(),
            });
            let p = self.new_part(Mode::NoIndent);
            self.parts.push(p);
        }
        self.groups.len() - 1
    }

    fn cur(&mut self) -> &mut String {
        self.cur_group();
        &mut self.parts.last_mut().unwrap().text
    }

    fn s(&mut self, s: &str) {
        self.cur().push_str(s);
    }

    fn c(&mut self, c: char) {
        self.cur().push(c);
    }

    fn int(&mut self, v: i64) {
        use std::fmt::Write;
        let _ = write!(self.cur(), "{v}");
    }

    /// `removeTrailingSpace`.
    fn strip(&mut self) {
        strip1(self.cur());
    }

    fn no_merge(&mut self) {
        self.cur_group();
        self.parts.last_mut().unwrap().mergeable = false;
    }

    /// `deparseAppendPart`.
    fn part(&mut self, dedup: bool) {
        let g = self.cur_group();
        if dedup && self.parts.last().is_some_and(|p| p.text.is_empty()) {
            self.parts.pop();
        }
        let p = self.new_part(self.groups[g].mode);
        self.parts.push(p);
    }

    fn comma_part(&mut self) {
        self.c(',');
        self.part(true);
    }

    /// `deparseAppendPartGroup`. The keyword's part is made here rather than when the level
    /// closes: the parts stay in the order they are written.
    fn group(&mut self, keyword: Option<&'static str>, mode: Mode) {
        let first = self.levels.last().map_or(0, |l| l.first_group);
        if first < self.groups.len() {
            self.strip();
        }
        self.groups.push(Group {
            keyword: keyword.is_some(),
            mode,
            first: self.parts.len(),
        });
        if let Some(k) = keyword {
            self.parts.push(Part {
                text: k.to_string(),
                indent: self.base(),
                mergeable: false,
            });
        }
        let p = self.new_part(mode);
        self.parts.push(p);
    }

    /// `deparseStateIncreaseNestingLevel`.
    fn up(&mut self) {
        let base = match self.levels.last() {
            Some(l) => {
                let base = l.base;
                let g = self.cur_group();
                self.parts.last_mut().unwrap().mergeable = false;
                base + INDENT
                    + if self.groups[g].mode == Mode::NoIndent {
                        0
                    } else {
                        INDENT
                    }
            }
            None => 0,
        };
        self.levels.push(Level {
            base,
            first_group: self.groups.len(),
        });
    }

    /// `deparseStateDecreaseNestingLevel`: merge each group's mergeable parts, put the
    /// keyword before its group (joined with the group's only part), and leave the parts to
    /// the enclosing level's current group. The level's parts are the list's suffix, so
    /// this compacts them in place.
    fn down(&mut self) {
        let lv = self.levels.pop().unwrap();
        let ng = self.groups.len();
        if lv.first_group < ng {
            let mut w = self.groups[lv.first_group].first;
            for gi in lv.first_group..ng {
                let g = self.groups[gi];
                let end = if gi + 1 < ng {
                    self.groups[gi + 1].first
                } else {
                    self.parts.len()
                };
                let mut r = g.first;
                let keyword = if g.keyword {
                    self.parts.swap(w, r);
                    r += 1;
                    w += 1;
                    Some(w - 1)
                } else {
                    None
                };
                let c = w;
                let merge = g.mode == Mode::Merge && end - r > 1;
                for r in r..end {
                    if merge && w > c {
                        let t = w - 1;
                        strip1(&mut self.parts[t].text);
                        let (tp, pp) = (&self.parts[t], &self.parts[r]);
                        if tp.mergeable
                            && pp.mergeable
                            && tp.indent + tp.text.len() + 1 + pp.text.len() <= MAX_LINE
                        {
                            let text = std::mem::take(&mut self.parts[r].text);
                            let t = &mut self.parts[t].text;
                            if !t.is_empty() && !t.ends_with('(') {
                                t.push(' ');
                            }
                            t.push_str(&text);
                            continue;
                        }
                    }
                    self.parts.swap(w, r);
                    w += 1;
                }
                if let Some(k) = keyword
                    && w > c
                    && (w - c == 1 || g.mode == Mode::NoIndent)
                {
                    let first = std::mem::take(&mut self.parts[c].text);
                    if !first.is_empty() {
                        let k = &mut self.parts[k].text;
                        k.push(' ');
                        k.push_str(&first);
                    }
                    self.parts[c..w].rotate_left(1);
                    w -= 1;
                }
            }
            self.parts.truncate(w);
            self.groups.truncate(lv.first_group);
        }
        if !self.levels.is_empty() {
            self.part(true);
            self.no_merge();
        }
    }

    /// `deparseEmit`: join the parts.
    fn finish(mut self) -> String {
        if self.parts.last().is_some_and(|p| p.text.is_empty()) {
            self.parts.pop();
        }
        let len = self.parts.iter().map(|p| p.text.len() + 1).sum();
        let mut out = String::with_capacity(len);
        let n = self.parts.len();
        for (i, p) in self.parts.iter().enumerate() {
            if matches!(p.text.as_bytes().first(), Some(b')' | b';')) {
                strip1(&mut out);
            }
            out.push_str(&p.text);
            strip1(&mut out);
            if i + 1 < n && !out.ends_with('(') {
                out.push(' ');
            }
        }
        out
    }

    /// `quote_identifier`.
    fn ident(&mut self, s: &str) {
        let out = self.cur();
        if !needs_quotes(s) {
            out.push_str(s);
            return;
        }
        out.push('"');
        for ch in s.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    }

    /// `quote_identifier` of a node's string field, which is NULL when empty.
    fn ident_field(&mut self, s: &str) -> R {
        if s.is_empty() {
            return Err(Unhandled);
        }
        self.ident(s);
        Ok(())
    }

    /// `deparseStringLiteral`: E'' when there is a backslash, quotes and backslashes doubled.
    fn literal(&mut self, s: &str) {
        let out = self.cur();
        let esc = s.contains('\\');
        if esc {
            out.push('E');
        }
        out.push('\'');
        for ch in s.chars() {
            if ch == '\'' || (esc && ch == '\\') {
                out.push(ch);
            }
            out.push(ch);
        }
        out.push('\'');
    }

    /// `any_name`: quoted names joined with dots.
    fn any_name(&mut self, names: &[Node]) -> R {
        for (i, n) in names.iter().enumerate() {
            self.ident(str_of(n)?);
            if i + 1 < names.len() {
                self.c('.');
            }
        }
        Ok(())
    }

    /// `name_list` / `columnList`: quoted names joined with commas.
    fn name_list(&mut self, names: &[Node]) -> R {
        for (i, n) in names.iter().enumerate() {
            self.ident(str_of(n)?);
            if i + 1 < names.len() {
                self.s(", ");
            }
        }
        Ok(())
    }
}

/// A `String` node's value (`strVal`, which errors on any other node).
fn str_of(n: &Node) -> Result<&str, Unhandled> {
    match &n.node {
        Some(NodeEnum::String(s)) => Ok(&s.sval),
        _ => Err(Unhandled),
    }
}

/// A `String` node's value, `None` for anything else.
fn str_opt(n: &Node) -> Option<&str> {
    match &n.node {
        Some(NodeEnum::String(s)) => Some(&s.sval),
        _ => None,
    }
}

/// A protobuf enum as libpg_query reads it: an unknown value (0 included) is its first.
fn en<E: TryFrom<i32>>(v: i32) -> E {
    match E::try_from(v) {
        Ok(e) if v != 0 => e,
        _ => E::try_from(1).ok().unwrap(),
    }
}

fn node<T>(n: &Option<Box<T>>) -> Result<&T, Unhandled> {
    n.as_deref().ok_or(Unhandled)
}

/// PostgreSQL's keyword categories (kwlist.h): `U`nreserved, `C`olumn name, `T`ype or
/// function name, `R`eserved.
fn keyword(s: &str) -> Option<u8> {
    static WORDS: OnceLock<Vec<(&'static str, u8)>> = OnceLock::new();
    let words = WORDS.get_or_init(|| {
        include_str!("../func/keywords.tsv")
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                Some((f.next()?, *f.next()?.as_bytes().first()?))
            })
            .collect()
    });
    words
        .binary_search_by(|(w, _)| (*w).cmp(s))
        .ok()
        .map(|i| words[i].1)
}

fn all_lower(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// `quote_identifier`: anything but a lower-case name that isn't a keyword (unreserved
/// ones aside) is quoted.
fn needs_quotes(s: &str) -> bool {
    let safe = s
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || *b == b'_')
        && all_lower(s);
    !safe || keyword(s).is_some_and(|k| k != b'U')
}

/// `isReservedKeyword`.
fn is_reserved(s: &str) -> bool {
    all_lower(s) && keyword(s) == Some(b'R')
}

