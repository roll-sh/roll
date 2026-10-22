//! `java.util.regex`: a backtracking regex engine with Java's syntax and semantics, and the
//! `Pattern`/`Matcher` API Painless exposes.
//!
//! Scripts observe Java's regex behaviour directly (leftmost-first alternation, backreferences,
//! lookaround, possessive and atomic groups, `\b` and `$` rules, UTF-16 indexes from
//! `start()`/`end()`, `split` dropping trailing empty strings, `PatternSyntaxException`
//! descriptions in compile errors), so the engine follows `java.util.regex.Pattern` rather than
//! a DFA-based crate. Patterns compile to a small instruction set run by a VM with an explicit
//! backtrack stack — deep inputs never recurse — and single-width repetitions (`a*`, `\d+`,
//! `.*?`) run as counted loops that backtrack by position instead of pushing a frame per
//! character. Lookaround and atomic groups run their body as a nested match whose surviving
//! capture-restore frames are kept, so later backtracking still restores captures.
//!
//! Matching runs over UTF-16 code units (Java's `CharSequence`); supplementary characters are
//! decoded as one code point. Elasticsearch's `script.painless.regex.limit-factor` wraps the
//! input in a counting `CharSequence`; here the VM counts character reads the same way and fails
//! with the same `circuit_breaking_exception` once `factor * length` reads are exceeded.
//! Unicode property tables (`\p{Lu}`, `\p{IsGreek}`) come from `regex-syntax`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::lookup::Builder;
use crate::types::cls;
use crate::value::{JList, Obj, Value};
use crate::vm::{Throw, Thrown, Vm, VR};

pub const UNIX_LINES: u32 = 0x01;
pub const CASE_INSENSITIVE: u32 = 0x02;
pub const COMMENTS: u32 = 0x04;
pub const MULTILINE: u32 = 0x08;
pub const LITERAL: u32 = 0x10;
pub const DOTALL: u32 = 0x20;
pub const UNICODE_CASE: u32 = 0x40;
pub const CANON_EQ: u32 = 0x80;
pub const UNICODE_CHARACTER_CLASS: u32 = 0x100;
/// Internal: Oniguruma's Ruby syntax as Elasticsearch's grok (Joni) reads it — `.` stops only
/// at `\n`, `^`/`$` are line anchors, `(?m)` is dot-all, `\h` is a hex digit, `\w`/`\d`/`\s`
/// are ASCII while `\b` and POSIX brackets are Unicode, group names may hold any character
/// and repeat, and a `{` that does not start an interval is literal. Errors use Joni's texts.
pub const ONIG: u32 = 0x1_0000;

/// Painless regex literal flag letters.
pub fn flag_of(c: char) -> Option<u32> {
    Some(match c {
        'c' => CANON_EQ,
        'i' => CASE_INSENSITIVE,
        'l' => LITERAL,
        'm' => MULTILINE,
        's' => DOTALL,
        'U' => UNICODE_CHARACTER_CLASS,
        'u' => UNICODE_CASE,
        'x' => COMMENTS,
        _ => return None,
    })
}

/// `PatternSyntaxException`: description and index.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternError {
    pub index: i32,
    pub description: String,
    /// Elasticsearch type of the exception (`pattern_syntax_exception`; Joni's
    /// `syntax_exception`/`value_exception` in [`ONIG`] mode, or `warning` for Joni warnings,
    /// which grok turns into errors).
    pub kind: &'static str,
}

impl PatternError {
    /// `PatternSyntaxException.getMessage()`: description, index, pattern and a caret.
    pub fn message(&self, pattern: &str) -> String {
        let mut s = self.description.clone();
        if self.index >= 0 {
            s.push_str(&format!(" near index {}", self.index));
        }
        s.push('\n');
        s.push_str(pattern);
        // the caret line only when the index falls inside the pattern
        if self.index >= 0 && (self.index as usize) < pattern.encode_utf16().count() {
            s.push('\n');
            for _ in 0..self.index {
                s.push(' ');
            }
            s.push('^');
        }
        s
    }
}

// ------------------------------------------------------------------------------ char sets

/// A set of code points as sorted, non-overlapping inclusive ranges.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CharSet {
    r: Vec<(u32, u32)>,
}

const MAX_CP: u32 = 0x10FFFF;

impl CharSet {
    fn new() -> CharSet {
        CharSet { r: Vec::new() }
    }
    fn of(ranges: &[(u32, u32)]) -> CharSet {
        let mut s = CharSet { r: ranges.to_vec() };
        s.normalize();
        s
    }
    fn add(&mut self, lo: u32, hi: u32) {
        self.r.push((lo, hi));
    }
    fn add_set(&mut self, o: &CharSet) {
        self.r.extend_from_slice(&o.r);
    }
    fn normalize(&mut self) {
        self.r.sort_unstable();
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(self.r.len());
        for &(lo, hi) in &self.r {
            if let Some(last) = out.last_mut()
                && lo <= last.1.saturating_add(1) {
                    last.1 = last.1.max(hi);
                    continue;
                }
            out.push((lo, hi));
        }
        self.r = out;
    }
    fn negate(&self) -> CharSet {
        let mut out = Vec::new();
        let mut next = 0u32;
        for &(lo, hi) in &self.r {
            if lo > next {
                out.push((next, lo - 1));
            }
            next = hi.saturating_add(1);
        }
        if next <= MAX_CP {
            out.push((next, MAX_CP));
        }
        CharSet { r: out }
    }
    fn intersect(&self, o: &CharSet) -> CharSet {
        let mut out = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < self.r.len() && j < o.r.len() {
            let (a, b) = (self.r[i], o.r[j]);
            let lo = a.0.max(b.0);
            let hi = a.1.min(b.1);
            if lo <= hi {
                out.push((lo, hi));
            }
            if a.1 < b.1 {
                i += 1;
            } else {
                j += 1;
            }
        }
        CharSet { r: out }
    }
    #[inline]
    fn contains(&self, c: u32) -> bool {
        if self.r.len() <= 4 {
            return self.r.iter().any(|&(lo, hi)| lo <= c && c <= hi);
        }
        self.r.binary_search_by(|&(lo, hi)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        }).is_ok()
    }
}

fn unicode_class(expr: &str) -> Option<CharSet> {
    let hir = regex_syntax::ParserBuilder::new().build().parse(expr).ok()?;
    match hir.kind() {
        regex_syntax::hir::HirKind::Class(regex_syntax::hir::Class::Unicode(c)) => {
            Some(CharSet::of(&c.ranges().iter().map(|r| (r.start() as u32, r.end() as u32)).collect::<Vec<_>>()))
        }
        regex_syntax::hir::HirKind::Literal(l) => {
            let s = std::str::from_utf8(&l.0).ok()?;
            let c = s.chars().next()? as u32;
            Some(CharSet::of(&[(c, c)]))
        }
        _ => None,
    }
}

fn ascii_set(spec: &str) -> CharSet {
    let mut s = CharSet::new();
    let b: Vec<char> = spec.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if i + 2 < b.len() && b[i + 1] == '-' {
            s.add(b[i] as u32, b[i + 2] as u32);
            i += 3;
        } else {
            s.add(b[i] as u32, b[i] as u32);
            i += 1;
        }
    }
    s.normalize();
    s
}

fn digit_set(ucc: bool) -> CharSet {
    if ucc {
        unicode_class(r"\p{Nd}").unwrap_or_default()
    } else {
        ascii_set("0-9")
    }
}

fn word_set(ucc: bool) -> CharSet {
    if ucc {
        let mut s = unicode_class(r"\p{Alphabetic}").unwrap_or_default();
        for p in [r"\p{Mn}", r"\p{Me}", r"\p{Mc}", r"\p{Nd}", r"\p{Pc}", r"\p{Join_Control}"] {
            if let Some(x) = unicode_class(p) {
                s.add_set(&x);
            }
        }
        s.normalize();
        s
    } else {
        ascii_set("a-zA-Z0-9_")
    }
}

fn space_set(ucc: bool) -> CharSet {
    if ucc {
        unicode_class(r"\p{White_Space}").unwrap_or_default()
    } else {
        CharSet::of(&[(9, 13), (32, 32)])
    }
}

fn hspace_set() -> CharSet {
    CharSet::of(&[(0x20, 0x20), (0x09, 0x09), (0xA0, 0xA0), (0x1680, 0x1680), (0x180e, 0x180e), (0x2000, 0x200a), (0x202f, 0x202f), (0x205f, 0x205f), (0x3000, 0x3000)])
}

fn vspace_set() -> CharSet {
    CharSet::of(&[(0x0A, 0x0D), (0x85, 0x85), (0x2028, 0x2029)])
}

/// `\p{name}` as Java resolves it.
fn property(name: &str, ucc: bool) -> Option<CharSet> {
    let posix = |n: &str| -> Option<CharSet> {
        Some(match n {
            "Lower" => {
                if ucc {
                    unicode_class(r"\p{Lowercase}")?
                } else {
                    ascii_set("a-z")
                }
            }
            "Upper" => {
                if ucc {
                    unicode_class(r"\p{Uppercase}")?
                } else {
                    ascii_set("A-Z")
                }
            }
            "ASCII" => CharSet::of(&[(0, 0x7F)]),
            "Alpha" => {
                if ucc {
                    unicode_class(r"\p{Alphabetic}")?
                } else {
                    ascii_set("a-zA-Z")
                }
            }
            "Digit" => digit_set(ucc),
            "Alnum" => {
                if ucc {
                    let mut s = unicode_class(r"\p{Alphabetic}")?;
                    s.add_set(&digit_set(true));
                    s.normalize();
                    s
                } else {
                    ascii_set("a-zA-Z0-9")
                }
            }
            "Punct" => {
                if ucc {
                    unicode_class(r"\p{P}")?
                } else {
                    CharSet::of(&[(0x21, 0x2F), (0x3A, 0x40), (0x5B, 0x60), (0x7B, 0x7E)])
                }
            }
            "Graph" => CharSet::of(&[(0x21, 0x7E)]),
            "Print" => CharSet::of(&[(0x20, 0x7E)]),
            "Blank" => CharSet::of(&[(0x09, 0x09), (0x20, 0x20)]),
            "Cntrl" => CharSet::of(&[(0, 0x1F), (0x7F, 0x7F)]),
            "XDigit" => ascii_set("0-9a-fA-F"),
            "Space" => space_set(ucc),
            _ => return None,
        })
    };
    if let Some(s) = posix(name) {
        return Some(s);
    }
    let java = match name {
        "javaLowerCase" => Some(r"\p{Lowercase}"),
        "javaUpperCase" => Some(r"\p{Uppercase}"),
        "javaWhitespace" => None,
        "javaMirrored" => None,
        "javaLetter" => Some(r"\p{L}"),
        "javaDigit" => Some(r"\p{Nd}"),
        "javaLetterOrDigit" => Some(r"[\p{L}\p{Nd}]"),
        "javaAlphabetic" => Some(r"\p{Alphabetic}"),
        "javaIdeographic" => Some(r"\p{Ideographic}"),
        "javaTitleCase" => Some(r"\p{Lt}"),
        "javaSpaceChar" => Some(r"\p{Z}"),
        "javaISOControl" => Some(r"[\x00-\x1F\x7F-\x9F]"),
        _ => None,
    };
    if name == "javaWhitespace" {
        return Some(CharSet::of(&[(9, 13), (0x1C, 0x20), (0x1680, 0x1680), (0x2000, 0x2006), (0x2008, 0x200A), (0x2028, 0x2029), (0x205F, 0x205F), (0x3000, 0x3000)]));
    }
    if let Some(j) = java {
        if let Some(cls) = unicode_class(j) {
            return Some(cls);
        }
        // a class expression rather than a single property
        let hir = regex_syntax::ParserBuilder::new().build().parse(j).ok()?;
        if let regex_syntax::hir::HirKind::Class(regex_syntax::hir::Class::Unicode(c)) = hir.kind() {
            return Some(CharSet::of(&c.ranges().iter().map(|r| (r.start() as u32, r.end() as u32)).collect::<Vec<_>>()));
        }
        return None;
    }
    // Unicode: IsX (script, category or binary property), InX (block, unsupported), key=value
    let (key, val) = match name.split_once('=') {
        Some((k, v)) => (Some(k), v),
        None => (None, name),
    };
    let expr = match key {
        Some("sc") | Some("script") => format!(r"\p{{Script={val}}}"),
        Some("gc") | Some("general_category") => format!(r"\p{{gc={val}}}"),
        Some("blk") | Some("block") => return None,
        Some(_) => return None,
        None => {
            if let Some(rest) = val.strip_prefix("Is") {
                format!(r"\p{{{rest}}}")
            } else if val.starts_with("In") {
                return None;
            } else {
                // a general category (`L`, `Lu`) or a POSIX-like name with an `Is` prefix
                if val.len() <= 2 {
                    format!(r"\p{{gc={val}}}")
                } else {
                    return None;
                }
            }
        }
    };
    unicode_class(&expr)
}

// ------------------------------------------------------------------------------ AST

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anchor {
    /// `^`
    Bol { multiline: bool, unix: bool },
    /// `$`
    Eol { multiline: bool, unix: bool },
    /// `\A`
    Begin,
    /// `\z`
    End,
    /// `\Z`
    EndOptNl { unix: bool },
    /// `\b`, `\B`
    Word { neg: bool, ucc: bool },
    /// `\G`
    LastMatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fold {
    None,
    Ascii,
    Unicode,
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Lit(u32, Fold),
    Set(Rc<CharSet>, Fold),
    Any,
    AnyNoNl { unix: bool },
    Anchor(Anchor),
    Group(Box<Node>, Option<usize>),
    Look { node: Box<Node>, behind: bool, neg: bool },
    Atomic(Box<Node>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    /// `by_number`: written as an interval (`{n,m}`), which Oniguruma's nested-repeat warnings
    /// ignore.
    Repeat { node: Box<Node>, min: u32, max: u32, kind: RepKind, by_number: bool },
    Backref(usize, Fold),
    /// `\R`: `\r\n` or one vertical whitespace character.
    LineBreak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepKind {
    Greedy,
    Lazy,
    Possessive,
}

const UNBOUNDED: u32 = u32::MAX;

/// Width range (in UTF-16 units) a node can match, for look-behind.
fn widths(n: &Node) -> (usize, Option<usize>) {
    match n {
        Node::Empty | Node::Anchor(_) | Node::Look { .. } => (0, Some(0)),
        Node::Lit(c, _) => {
            let w = if *c > 0xFFFF { 2 } else { 1 };
            (w, Some(w))
        }
        Node::Set(..) | Node::Any | Node::AnyNoNl { .. } => (1, Some(2)),
        Node::LineBreak => (1, Some(2)),
        Node::Group(n, _) | Node::Atomic(n) => widths(n),
        Node::Concat(v) => {
            let mut lo = 0;
            let mut hi = Some(0);
            for x in v {
                let (a, b) = widths(x);
                lo += a;
                hi = match (hi, b) {
                    (Some(h), Some(b)) => Some(h + b),
                    _ => None,
                };
            }
            (lo, hi)
        }
        Node::Alt(v) => {
            let mut lo = usize::MAX;
            let mut hi = Some(0);
            for x in v {
                let (a, b) = widths(x);
                lo = lo.min(a);
                hi = match (hi, b) {
                    (Some(h), Some(b)) => Some(h.max(b)),
                    _ => None,
                };
            }
            (if lo == usize::MAX { 0 } else { lo }, hi)
        }
        Node::Repeat { node, min, max, .. } => {
            let (a, b) = widths(node);
            let lo = a.saturating_mul(*min as usize);
            let hi = match (b, *max) {
                (_, UNBOUNDED) => {
                    if b == Some(0) {
                        Some(0)
                    } else {
                        None
                    }
                }
                (Some(b), m) => Some(b.saturating_mul(m as usize)),
                (None, _) => None,
            };
            (lo, hi)
        }
        Node::Backref(..) => (0, None),
    }
}

// ------------------------------------------------------------------------------ parser

struct Parser<'a> {
    p: &'a [char],
    i: usize,
    flags: u32,
    groups: usize,
    names: Vec<(String, usize)>,
    /// Joni's warnings in [`ONIG`] mode (grok in runtime fields refuses patterns with any).
    warnings: Vec<String>,
}

type PR<T> = Result<T, PatternError>;

impl Parser<'_> {
    fn err(&self, desc: &str, index: usize) -> PatternError {
        PatternError { index: index as i32, description: desc.to_string(), kind: "pattern_syntax_exception" }
    }

    fn onig(&self) -> bool {
        self.flags & ONIG != 0
    }

    /// A Joni error (`syntax_exception` or `value_exception`).
    fn joni(&self, kind: &'static str, desc: &str) -> PatternError {
        PatternError { index: -1, description: desc.to_string(), kind }
    }

    fn fold(&self) -> Fold {
        if self.flags & CASE_INSENSITIVE == 0 {
            Fold::None
        } else if self.flags & UNICODE_CASE != 0 {
            Fold::Unicode
        } else {
            Fold::Ascii
        }
    }

    fn ucc(&self) -> bool {
        self.flags & UNICODE_CHARACTER_CLASS != 0
    }

    /// Next char, skipping whitespace and comments in COMMENTS mode.
    fn peek(&mut self) -> Option<char> {
        if self.flags & COMMENTS != 0 {
            self.skip_comments();
        }
        self.p.get(self.i).copied()
    }

    fn skip_comments(&mut self) {
        while let Some(&c) = self.p.get(self.i) {
            if c.is_whitespace() {
                self.i += 1;
            } else if c == '#' {
                while let Some(&c) = self.p.get(self.i) {
                    self.i += 1;
                    if c == '\n' || c == '\r' || c == '\u{85}' || c == '\u{2028}' || c == '\u{2029}' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    fn raw_next(&mut self) -> Option<char> {
        let c = self.p.get(self.i).copied();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    fn parse(&mut self) -> PR<Node> {
        let n = self.expr()?;
        if let Some(c) = self.peek()
            && c == ')' {
                if self.onig() {
                    return Err(self.joni("syntax_exception", "unmatched close parenthesis"));
                }
                return Err(self.err("Unmatched closing ')'", self.i.saturating_sub(1)));
            }
        Ok(n)
    }

    fn expr(&mut self) -> PR<Node> {
        let mut alts = vec![self.sequence()?];
        while self.peek() == Some('|') {
            self.i += 1;
            alts.push(self.sequence()?);
        }
        Ok(if alts.len() == 1 { alts.pop().unwrap() } else { Node::Alt(alts) })
    }

    fn sequence(&mut self) -> PR<Node> {
        let mut items: Vec<Node> = Vec::new();
        while let Some(c) = self.peek() {
            let atom = match c {
                '|' | ')' => break,
                '(' => {
                    self.i += 1;
                    let g = self.group()?;
                    match g {
                        Some(n) => n,
                        None => continue,
                    }
                }
                '[' => {
                    self.i += 1;
                    let set = self.class()?;
                    Node::Set(Rc::new(set), self.fold())
                }
                '\\' => {
                    self.i += 1;
                    match self.escape(false)? {
                        Esc::Node(n) => n,
                        Esc::Quote(s) => {
                            let fold = self.fold();
                            for ch in s {
                                items.push(Node::Lit(ch as u32, fold));
                            }
                            continue;
                        }
                    }
                }
                '^' => {
                    self.i += 1;
                    // Ruby's `^`/`$` are line anchors (lines end at `\n` only)
                    let onig = self.onig();
                    Node::Anchor(Anchor::Bol { multiline: onig || self.flags & MULTILINE != 0, unix: onig || self.flags & UNIX_LINES != 0 })
                }
                '$' => {
                    self.i += 1;
                    let onig = self.onig();
                    Node::Anchor(Anchor::Eol { multiline: onig || self.flags & MULTILINE != 0, unix: onig || self.flags & UNIX_LINES != 0 })
                }
                '.' => {
                    self.i += 1;
                    if self.flags & DOTALL != 0 {
                        Node::Any
                    } else {
                        Node::AnyNoNl { unix: self.onig() || self.flags & UNIX_LINES != 0 }
                    }
                }
                '*' | '+' | '?' => {
                    if self.onig() {
                        return Err(self.joni("syntax_exception", "target of repeat operator is not specified"));
                    }
                    return Err(self.err(&format!("Dangling meta character '{c}'"), self.i));
                }
                '{' if self.onig() => {
                    self.i += 1;
                    Node::Lit('{' as u32, self.fold())
                }
                '{' => return Err(self.err("Illegal repetition", self.i)),
                _ => {
                    self.i += 1;
                    Node::Lit(c as u32, self.fold())
                }
            };
            let atom = self.closure(atom)?;
            items.push(atom);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap(),
            _ => Node::Concat(items),
        })
    }

    fn closure(&mut self, atom: Node) -> PR<Node> {
        let start = self.i;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, UNBOUNDED)
            }
            Some('+') => {
                self.i += 1;
                (1, UNBOUNDED)
            }
            Some('?') => {
                self.i += 1;
                (0, 1)
            }
            Some('{') if self.onig() => match self.onig_interval()? {
                Some(r) => r,
                None => return Ok(atom),
            },
            Some('{') => {
                self.i += 1;
                let num = |p: &mut Parser| -> Option<u32> {
                    let s = p.i;
                    let mut v: u64 = 0;
                    while let Some(&c) = p.p.get(p.i) {
                        if let Some(d) = c.to_digit(10) {
                            v = v * 10 + d as u64;
                            if v > i32::MAX as u64 {
                                v = i32::MAX as u64 + 1;
                            }
                            p.i += 1;
                        } else {
                            break;
                        }
                    }
                    if p.i == s {
                        None
                    } else {
                        Some(v.min(u32::MAX as u64 - 1) as u32)
                    }
                };
                let Some(lo) = num(self) else {
                    return Err(self.err("Illegal repetition", start));
                };
                let hi = if self.p.get(self.i) == Some(&',') {
                    self.i += 1;
                    if self.p.get(self.i) == Some(&'}') { UNBOUNDED } else { num(self).unwrap_or(UNBOUNDED) }
                } else {
                    lo
                };
                if self.p.get(self.i) != Some(&'}') {
                    return Err(self.err("Unclosed counted closure", self.i));
                }
                self.i += 1;
                if hi < lo || lo as u64 > i32::MAX as u64 {
                    return Err(self.err("Illegal repetition range", start));
                }
                (lo, hi)
            }
            _ => return Ok(atom),
        };
        let kind = match self.peek() {
            Some('?') => {
                self.i += 1;
                RepKind::Lazy
            }
            Some('+') if !(self.onig() && self.p.get(start) == Some(&'{')) => {
                self.i += 1;
                RepKind::Possessive
            }
            _ => RepKind::Greedy,
        };
        let by_number = self.p.get(start) == Some(&'{');
        let node = Node::Repeat { node: Box::new(atom), min, max, kind, by_number };
        if self.onig() {
            self.nested_repeat_warning(&node);
            // Ruby applies a repeat to a repeat (`a**`)
            if matches!(self.peek(), Some('*' | '+' | '?' | '{')) {
                return self.closure(node);
            }
        }
        Ok(node)
    }

    /// Oniguruma's warning for a repeat applied to a repeat (`a?*`, `(?:a*)*`), which grok
    /// turns into an error: `ReduceTypeTable` decides between "redundant" and "replaced".
    fn nested_repeat_warning(&mut self, outer: &Node) {
        fn popular(min: u32, max: u32, kind: RepKind) -> Option<usize> {
            let base = match kind {
                RepKind::Greedy => 0,
                RepKind::Lazy => 3,
                RepKind::Possessive => return None,
            };
            Some(base + match (min, max) {
                (0, 1) => 0,
                (0, UNBOUNDED) => 1,
                (1, UNBOUNDED) => 2,
                _ => return None,
            })
        }
        let Node::Repeat { node, min, max, kind, by_number: false } = outer else { return };
        let Some(out_q) = popular(*min, *max, *kind) else { return };
        let mut inner = &**node;
        while let Node::Group(n, None) = inner {
            inner = n;
        }
        let Node::Repeat { min, max, kind, by_number: false, .. } = inner else { return };
        let Some(in_q) = popular(*min, *max, *kind) else { return };
        const Q: [&str; 6] = ["?", "*", "+", "??", "*?", "+?"];
        // Joni's table (rows: inner, columns: outer); "" keeps the repeats, "-" drops the
        // inner one
        const TABLE: [[&str; 6]; 6] = [
            ["-", "*", "*", "??", "*?", ""],
            ["-", "-", "-", "+ and ??", "+ and ??", "-"],
            ["*", "*", "-", "", "+ and ??", "-"],
            ["-", "*?", "*?", "-", "*?", "*?"],
            ["-", "-", "-", "-", "-", "-"],
            ["", "", "-", "*?", "*?", "-"],
        ];
        let src: String = self.p.iter().collect();
        let msg = match TABLE[in_q][out_q] {
            "" => return,
            "-" => format!("regular expression has redundant nested repeat operator {} /{src}/", Q[in_q]),
            r => format!("nested repeat operator '{}' and '{}' was replaced with '{r}' in regular expression /{src}/", Q[in_q], Q[out_q]),
        };
        self.warnings.push(msg);
    }

    /// An Oniguruma interval at `{`: `{n}`, `{n,}`, `{n,m}` or `{,m}`; anything else leaves the
    /// `{` to be read as a literal (`None`).
    fn onig_interval(&mut self) -> PR<Option<(u32, u32)>> {
        let start = self.i;
        let mut j = self.i + 1;
        let digits = |j: &mut usize| -> Option<u32> {
            let s = *j;
            let mut v: u64 = 0;
            while let Some(d) = self.p.get(*j).and_then(|c| c.to_digit(10)) {
                v = (v * 10 + d as u64).min(100_001);
                *j += 1;
            }
            (*j > s).then_some(v as u32)
        };
        let lo = digits(&mut j);
        let (lo, hi) = if self.p.get(j) == Some(&',') {
            j += 1;
            let hi = digits(&mut j);
            if lo.is_none() && hi.is_none() {
                return Ok(None);
            }
            (lo.unwrap_or(0), hi.unwrap_or(UNBOUNDED))
        } else {
            match lo {
                Some(n) => (n, n),
                None => return Ok(None),
            }
        };
        if self.p.get(j) != Some(&'}') {
            return Ok(None);
        }
        if lo > 100_000 || (hi != UNBOUNDED && hi > 100_000) {
            return Err(self.joni("value_exception", "too big number for repeat range"));
        }
        if hi < lo {
            return Err(self.joni("value_exception", "upper is smaller than lower in repeat range"));
        }
        let _ = start;
        self.i = j + 1;
        Ok(Some((lo, hi)))
    }

    /// After `(`; `None` for a flags-only group like `(?i)`.
    fn group(&mut self) -> PR<Option<Node>> {
        let save_flags = self.flags;
        let open = self.i - 1;
        let node;
        if self.p.get(self.i) == Some(&'?') {
            self.i += 1;
            let Some(c) = self.raw_next() else {
                return Err(self.err("Unknown group type", self.i));
            };
            match c {
                ':' => {
                    let n = self.expr()?;
                    node = n;
                }
                '=' | '!' => {
                    let n = self.expr()?;
                    node = Node::Look { node: Box::new(n), behind: false, neg: c == '!' };
                }
                '>' => {
                    let n = self.expr()?;
                    node = Node::Atomic(Box::new(n));
                }
                '<' => {
                    let Some(&c2) = self.p.get(self.i) else {
                        return Err(self.err("capturing group name does not start with a Latin letter", self.i));
                    };
                    if c2 == '=' || c2 == '!' {
                        self.i += 1;
                        let n = self.expr()?;
                        let (_, hi) = widths(&n);
                        if hi.is_none() {
                            return Err(self.err("Look-behind group does not have an obvious maximum length", self.i));
                        }
                        node = Node::Look { node: Box::new(n), behind: true, neg: c2 == '!' };
                    } else if self.onig() {
                        let name = self.onig_group_name()?;
                        self.groups += 1;
                        let idx = self.groups;
                        self.names.push((name, idx));
                        let n = self.expr()?;
                        node = Node::Group(Box::new(n), Some(idx));
                    } else if c2.is_ascii_alphabetic() {
                        let name = self.group_name()?;
                        if self.names.iter().any(|(n, _)| *n == name) {
                            return Err(self.err(&format!("Named capturing group <{name}> is already defined"), self.i - 1));
                        }
                        self.groups += 1;
                        let idx = self.groups;
                        self.names.push((name, idx));
                        let n = self.expr()?;
                        node = Node::Group(Box::new(n), Some(idx));
                    } else {
                        return Err(self.err("capturing group name does not start with a Latin letter", self.i));
                    }
                }
                _ => {
                    // inline flags
                    self.i -= 1;
                    let mut on = true;
                    loop {
                        let Some(c) = self.raw_next() else {
                            return Err(self.err("Unknown inline modifier", self.i));
                        };
                        // Ruby's options: `m` is dot-all, there is no `s`
                        let f = match c {
                            'i' if self.onig() => CASE_INSENSITIVE | UNICODE_CASE,
                            'm' if self.onig() => DOTALL,
                            'x' if self.onig() => COMMENTS,
                            c if self.onig() && !matches!(c, '-' | ')' | ':') => return Err(self.joni("syntax_exception", "undefined group option")),
                            'i' => CASE_INSENSITIVE,
                            'm' => MULTILINE,
                            's' => DOTALL,
                            'd' => UNIX_LINES,
                            'u' => UNICODE_CASE,
                            'c' => CANON_EQ,
                            'x' => COMMENTS,
                            'U' => UNICODE_CHARACTER_CLASS | UNICODE_CASE,
                            '-' => {
                                on = false;
                                continue;
                            }
                            ')' => return Ok(None),
                            ':' => break,
                            _ => return Err(self.err("Unknown inline modifier", self.i - 1)),
                        };
                        if on {
                            self.flags |= f;
                        } else {
                            self.flags &= !f;
                        }
                    }
                    let n = self.expr()?;
                    self.flags = save_flags;
                    node = Node::Group(Box::new(n), None);
                    if self.peek() != Some(')') {
                        if self.onig() {
                            return Err(self.joni("syntax_exception", "end pattern with unmatched parenthesis"));
                        }
                        return Err(self.err("Unclosed group", self.p.len()));
                    }
                    self.i += 1;
                    return Ok(Some(node));
                }
            }
        } else {
            self.groups += 1;
            let idx = self.groups;
            let n = self.expr()?;
            node = Node::Group(Box::new(n), Some(idx));
        }
        let _ = open;
        self.flags = save_flags;
        if self.peek() != Some(')') {
            if self.onig() {
                return Err(self.joni("syntax_exception", "end pattern with unmatched parenthesis"));
            }
            return Err(self.err("Unclosed group", self.p.len()));
        }
        self.i += 1;
        Ok(Some(node))
    }

    /// A group name in [`ONIG`] mode: anything up to `>` (grok names groups `IP:client`).
    fn onig_group_name(&mut self) -> PR<String> {
        let mut s = String::new();
        while let Some(&c) = self.p.get(self.i) {
            if c == '>' {
                self.i += 1;
                if s.is_empty() {
                    return Err(self.joni("value_exception", "group name is empty"));
                }
                return Ok(s);
            }
            if c == ')' {
                break;
            }
            s.push(c);
            self.i += 1;
        }
        Err(self.joni("value_exception", &format!("invalid group name <{s}>")))
    }

    fn group_name(&mut self) -> PR<String> {
        let mut s = String::new();
        while let Some(&c) = self.p.get(self.i) {
            if c.is_ascii_alphanumeric() {
                s.push(c);
                self.i += 1;
            } else {
                break;
            }
        }
        if self.p.get(self.i) != Some(&'>') {
            return Err(self.err("named capturing group is missing trailing '>'", self.i));
        }
        self.i += 1;
        Ok(s)
    }

    /// After `[`.
    fn class(&mut self) -> PR<CharSet> {
        let start = self.i;
        let mut neg = false;
        if self.peek() == Some('^') {
            self.i += 1;
            neg = true;
        }
        let mut acc = CharSet::new();
        let mut inter: Option<CharSet> = None;
        let mut first = true;
        loop {
            let Some(c) = self.peek() else {
                if self.onig() {
                    return Err(self.joni("syntax_exception", "premature end of char-class"));
                }
                return Err(self.err("Unclosed character class", self.p.len().saturating_sub(1)));
            };
            if c == '[' && self.onig() && self.p.get(self.i + 1) == Some(&':')
                && let Some(posix) = self.posix_bracket()
            {
                acc.add_set(&posix);
                first = false;
                continue;
            }
            match c {
                ']' if !first || !acc.r.is_empty() => {
                    self.i += 1;
                    break;
                }
                '[' => {
                    self.i += 1;
                    let inner = self.class()?;
                    acc.add_set(&inner);
                }
                '&' if self.p.get(self.i + 1) == Some(&'&') => {
                    self.i += 2;
                    acc.normalize();
                    let cur = std::mem::take(&mut acc);
                    inter = Some(match inter {
                        Some(i) => i.intersect(&cur),
                        None => cur,
                    });
                    // the right operand: a nested class or the rest up to `]`/`&&`
                    let mut right = CharSet::new();
                    loop {
                        match self.peek() {
                            None => return Err(self.err("Unclosed character class", self.p.len().saturating_sub(1))),
                            Some(']') => break,
                            Some('&') if self.p.get(self.i + 1) == Some(&'&') => break,
                            Some('[') => {
                                self.i += 1;
                                let inner = self.class()?;
                                right.add_set(&inner);
                            }
                            Some(_) => {
                                let r = self.range()?;
                                right.add_set(&r);
                            }
                        }
                    }
                    right.normalize();
                    let i = inter.take().unwrap();
                    inter = Some(i.intersect(&right));
                    first = false;
                    // continue: a following `&&` intersects further
                    continue;
                }
                _ => {
                    let r = self.range()?;
                    acc.add_set(&r);
                }
            }
            first = false;
        }
        acc.normalize();
        let mut set = match inter {
            Some(i) => {
                if acc.r.is_empty() {
                    i
                } else {
                    i.intersect(&acc)
                }
            }
            None => acc,
        };
        set.normalize();
        let _ = start;
        if neg {
            set = set.negate();
        }
        Ok(set)
    }

    /// `[:name:]` / `[:^name:]` inside a class in [`ONIG`] mode (Unicode, as Oniguruma's UTF-8
    /// encoding defines them); `None` leaves the `[` to the class parser.
    fn posix_bracket(&mut self) -> Option<CharSet> {
        let start = self.i;
        let mut j = self.i + 2;
        let neg = self.p.get(j) == Some(&'^');
        if neg {
            j += 1;
        }
        let ns = j;
        while self.p.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
            j += 1;
        }
        if self.p.get(j) != Some(&':') || self.p.get(j + 1) != Some(&']') {
            return None;
        }
        let name: String = self.p[ns..j].iter().collect();
        let class = match name.as_str() {
            "alnum" => r"[\p{L}\p{M}\p{Nd}]",
            "alpha" => r"[\p{L}\p{M}]",
            "ascii" => r"[\x00-\x7F]",
            "blank" => r"[\p{Zs}\t]",
            "cntrl" => r"[\p{Cc}\p{Cf}]",
            "digit" => r"[\p{Nd}]",
            "graph" => r"[^\p{Z}\p{C}]",
            "lower" => r"[\p{Ll}]",
            "print" => r"[^\p{Zl}\p{Zp}\p{C}]",
            "punct" => r"[\p{P}]",
            "space" => r"[\s\p{Z}\x85]",
            "upper" => r"[\p{Lu}]",
            "xdigit" => r"[0-9A-Fa-f]",
            "word" => r"[\p{L}\p{M}\p{Nd}\p{Pc}]",
            _ => return None,
        };
        let set = unicode_class(class)?;
        self.i = j + 2;
        let _ = start;
        Some(if neg { set.negate() } else { set })
    }

    /// One class item: a char, an escape or a range.
    fn range(&mut self) -> PR<CharSet> {
        let at = self.i;
        let lo = match self.class_atom()? {
            ClassAtom::Char(c) => c,
            ClassAtom::Set(s) => return Ok(s),
        };
        if self.peek() == Some('-') {
            let save = self.i;
            self.i += 1;
            match self.peek() {
                Some(']') | Some('[') => {
                    self.i = save;
                }
                // Java reads the end-of-pattern sentinel (0) as the range end
                None => return Err(self.err("Illegal character range", self.p.len())),
                Some(_) => {
                    let hi = match self.class_atom()? {
                        ClassAtom::Char(c) => c,
                        ClassAtom::Set(_) => return Err(self.err("Illegal character range", self.i - 1)),
                    };
                    if hi < lo {
                        return Err(self.err("Illegal character range", self.i - 1));
                    }
                    let _ = at;
                    return Ok(CharSet::of(&[(lo, hi)]));
                }
            }
        }
        Ok(CharSet::of(&[(lo, lo)]))
    }

    fn class_atom(&mut self) -> PR<ClassAtom> {
        let Some(c) = self.next() else {
            return Err(self.err("Unclosed character class", self.p.len().saturating_sub(1)));
        };
        if c == '\\' {
            return match self.escape(true)? {
                Esc::Node(Node::Lit(c, _)) => Ok(ClassAtom::Char(c)),
                Esc::Node(Node::Set(s, _)) => Ok(ClassAtom::Set((*s).clone())),
                Esc::Quote(chars) => {
                    let mut s = CharSet::new();
                    for ch in chars {
                        s.add(ch as u32, ch as u32);
                    }
                    s.normalize();
                    Ok(ClassAtom::Set(s))
                }
                _ => Err(self.err("Illegal/unsupported escape sequence", self.i - 1)),
            };
        }
        let mut cp = c as u32;
        let _ = &mut cp;
        Ok(ClassAtom::Char(cp))
    }

    /// After a backslash.
    fn escape(&mut self, in_class: bool) -> PR<Esc> {
        let Some(c) = self.raw_next() else {
            if self.onig() {
                return Err(self.joni("syntax_exception", "end pattern at escape"));
            }
            return Err(self.err("Unexpected internal error", self.i));
        };
        if self.onig() {
            let fold = self.fold();
            let hex = || {
                let mut s = CharSet::new();
                s.add('0' as u32, '9' as u32);
                s.add('A' as u32, 'F' as u32);
                s.add('a' as u32, 'f' as u32);
                s.normalize();
                s
            };
            match c {
                'h' => return Ok(Esc::Node(Node::Set(Rc::new(hex()), fold))),
                'H' => return Ok(Esc::Node(Node::Set(Rc::new(hex().negate()), fold))),
                // Oniguruma's word boundary knows Unicode letters even where `\w` does not
                'b' if !in_class => return Ok(Esc::Node(Node::Anchor(Anchor::Word { neg: false, ucc: true }))),
                'B' if !in_class => return Ok(Esc::Node(Node::Anchor(Anchor::Word { neg: true, ucc: true }))),
                'u' => return Ok(Esc::Node(Node::Lit('u' as u32, fold))),
                // Ruby syntax has no `\Q...\E` quoting: Joni reads `\Q` as `Q` and warns
                // about `\E`
                'Q' => return Ok(Esc::Node(Node::Lit('Q' as u32, fold))),
                'E' => {
                    self.warnings.push("Unknown escape \\E is ignored".to_string());
                    return Ok(Esc::Node(Node::Lit('E' as u32, fold)));
                }
                _ => {}
            }
        }
        let fold = self.fold();
        let ucc = self.ucc();
        let lit = |c: u32| Ok(Esc::Node(Node::Lit(c, fold)));
        let set = |s: CharSet| Ok(Esc::Node(Node::Set(Rc::new(s), fold)));
        match c {
            '0' => {
                let mut v: u32 = 0;
                let mut n = 0;
                while n < 3 {
                    match self.p.get(self.i).and_then(|c| c.to_digit(8)) {
                        Some(d) if v * 8 + d <= 0o377 => {
                            v = v * 8 + d;
                            self.i += 1;
                            n += 1;
                        }
                        _ => break,
                    }
                }
                if n == 0 {
                    return Err(self.err("Illegal octal escape sequence", self.i));
                }
                lit(v)
            }
            '1'..='9' if !in_class => {
                let mut num = c.to_digit(10).unwrap() as usize;
                while let Some(d) = self.p.get(self.i).and_then(|c| c.to_digit(10)) {
                    let next = num * 10 + d as usize;
                    if next > self.groups {
                        break;
                    }
                    num = next;
                    self.i += 1;
                }
                Ok(Esc::Node(Node::Backref(num, fold)))
            }
            'k' if !in_class => {
                if self.p.get(self.i) != Some(&'<') {
                    return Err(self.err("\\k is not followed by '<' for named capturing group", self.i));
                }
                self.i += 1;
                let name = self.group_name()?;
                match self.names.iter().find(|(n, _)| *n == name) {
                    Some((_, idx)) => Ok(Esc::Node(Node::Backref(*idx, fold))),
                    None => Err(self.err(&format!("named capturing group <{name}> does not exist"), self.i - 1)),
                }
            }
            'a' => lit(7),
            'e' => lit(27),
            'f' => lit(12),
            'n' => lit(10),
            'r' => lit(13),
            't' => lit(9),
            'c' => match self.raw_next() {
                Some(x) => lit(x as u32 ^ 64),
                None => Err(self.err("Illegal control escape sequence", self.i)),
            },
            'x' => {
                if self.p.get(self.i) == Some(&'{') {
                    self.i += 1;
                    let s = self.i;
                    while self.p.get(self.i).is_some_and(|c| c.is_ascii_hexdigit()) {
                        self.i += 1;
                    }
                    let hex: String = self.p[s..self.i].iter().collect();
                    if hex.is_empty() || self.p.get(self.i) != Some(&'}') {
                        return Err(self.err("Unclosed hexadecimal escape sequence", self.i));
                    }
                    self.i += 1;
                    match u32::from_str_radix(&hex, 16) {
                        Ok(v) if v <= MAX_CP => lit(v),
                        _ => Err(self.err("Hexadecimal codepoint is too big", self.i - 1)),
                    }
                } else {
                    let h: String = self.p.get(self.i..self.i + 2).map(|s| s.iter().collect()).unwrap_or_default();
                    if h.len() == 2 && h.chars().all(|c| c.is_ascii_hexdigit()) {
                        self.i += 2;
                        lit(u32::from_str_radix(&h, 16).unwrap())
                    } else {
                        Err(self.err("Illegal hexadecimal escape sequence", self.i))
                    }
                }
            }
            'u' => {
                let h: String = self.p.get(self.i..self.i + 4).map(|s| s.iter().collect()).unwrap_or_default();
                if h.len() == 4 && h.chars().all(|c| c.is_ascii_hexdigit()) {
                    self.i += 4;
                    let mut v = u32::from_str_radix(&h, 16).unwrap();
                    // a surrogate pair written as two escapes is one code point
                    if (0xD800..0xDC00).contains(&v) && self.p.get(self.i) == Some(&'\\') && self.p.get(self.i + 1) == Some(&'u') {
                        let h2: String = self.p.get(self.i + 2..self.i + 6).map(|s| s.iter().collect()).unwrap_or_default();
                        if let Ok(lo) = u32::from_str_radix(&h2, 16)
                            && (0xDC00..0xE000).contains(&lo) {
                                self.i += 6;
                                v = 0x10000 + ((v - 0xD800) << 10) + (lo - 0xDC00);
                            }
                    }
                    lit(v)
                } else {
                    Err(self.err("Illegal Unicode escape sequence", self.i))
                }
            }
            'd' => set(digit_set(ucc)),
            'D' => set(digit_set(ucc).negate()),
            's' => set(space_set(ucc)),
            'S' => set(space_set(ucc).negate()),
            'w' => set(word_set(ucc)),
            'W' => set(word_set(ucc).negate()),
            'h' => set(hspace_set()),
            'H' => set(hspace_set().negate()),
            'v' => set(vspace_set()),
            'V' => set(vspace_set().negate()),
            'p' | 'P' => {
                let name = if self.p.get(self.i) == Some(&'{') {
                    self.i += 1;
                    let s = self.i;
                    while self.p.get(self.i).is_some_and(|c| *c != '}') {
                        self.i += 1;
                    }
                    if self.p.get(self.i) != Some(&'}') {
                        return Err(self.err("Unclosed character family", self.i));
                    }
                    let n: String = self.p[s..self.i].iter().collect();
                    self.i += 1;
                    n
                } else {
                    match self.raw_next() {
                        Some(c) => c.to_string(),
                        None => return Err(self.err("Illegal character family", self.i)),
                    }
                };
                let s = match property(&name, ucc || self.onig()) {
                    Some(s) => s,
                    None => return Err(self.err(&format!("Unknown character property name {{{name}}}"), self.i - 1)),
                };
                set(if c == 'P' { s.negate() } else { s })
            }
            'Q' => {
                let mut chars = Vec::new();
                while let Some(&c) = self.p.get(self.i) {
                    if c == '\\' && self.p.get(self.i + 1) == Some(&'E') {
                        self.i += 2;
                        return Ok(Esc::Quote(chars));
                    }
                    chars.push(c);
                    self.i += 1;
                }
                Ok(Esc::Quote(chars))
            }
            'E' => Ok(Esc::Quote(Vec::new())),
            'b' if !in_class => Ok(Esc::Node(Node::Anchor(Anchor::Word { neg: false, ucc }))),
            'B' if !in_class => Ok(Esc::Node(Node::Anchor(Anchor::Word { neg: true, ucc }))),
            'A' if !in_class => Ok(Esc::Node(Node::Anchor(Anchor::Begin))),
            'z' if !in_class => Ok(Esc::Node(Node::Anchor(Anchor::End))),
            'Z' if !in_class => Ok(Esc::Node(Node::Anchor(Anchor::EndOptNl { unix: self.flags & UNIX_LINES != 0 }))),
            'G' if !in_class => Ok(Esc::Node(Node::Anchor(Anchor::LastMatch))),
            'R' if !in_class => Ok(Esc::Node(Node::LineBreak)),
            'X' if !in_class => {
                // a grapheme cluster, approximated as a code point and its combining marks
                let marks = unicode_class(r"[\p{M}]").unwrap_or_default();
                Ok(Esc::Node(Node::Concat(vec![
                    Node::Any,
                    Node::Repeat { node: Box::new(Node::Set(Rc::new(marks), Fold::None)), min: 0, max: UNBOUNDED, kind: RepKind::Possessive, by_number: false },
                ])))
            }
            c if c.is_ascii_alphabetic() => Err(self.err("Illegal/unsupported escape sequence", self.i - 1)),
            c => lit(c as u32),
        }
    }
}

enum Esc {
    Node(Node),
    Quote(Vec<char>),
}

enum ClassAtom {
    Char(u32),
    Set(CharSet),
}

// ------------------------------------------------------------------------------ program

#[derive(Clone, Debug)]
enum Item {
    Lit(u32, Fold),
    Set(u32, Fold),
    Any,
    AnyNoNl { unix: bool },
}

#[derive(Clone, Debug)]
enum Inst {
    Item(Item),
    Anchor(Anchor),
    /// Try `.0` first, then `.1`.
    Split(u32, u32),
    Jmp(u32),
    Save(u32),
    Backref(u32, Fold),
    LineBreak,
    /// A repetition of a single-width item: counted, then backtracked by position.
    Repeat { item: Item, min: u32, max: u32, greedy: bool },
    /// Loop head for general repetitions: `reg` counts iterations, `pos` remembers where the
    /// current iteration started (an iteration that matched nothing ends the loop).
    LoopInit { reg: u32 },
    Loop { reg: u32, pos: u32, min: u32, max: u32, greedy: bool, exit: u32 },
    LoopEnd { head: u32 },
    /// Nested match of `[pc+1, end)` (ended by `SubMatch`).
    Look { behind: Option<(u32, u32)>, neg: bool, end: u32 },
    Atomic { end: u32 },
    SubMatch,
    Match,
}

/// A compiled pattern. Immutable and shareable across threads.
#[derive(Debug)]
pub struct Regex {
    source: String,
    flags: u32,
    prog: Vec<Inst>,
    sets: Vec<CharSet>,
    ngroups: usize,
    nregs: usize,
    names: Vec<(String, usize)>,
    warnings: Vec<String>,
}

struct Compiler {
    prog: Vec<Inst>,
    sets: Vec<CharSet>,
    nregs: u32,
}

impl Compiler {
    fn emit(&mut self, i: Inst) -> u32 {
        self.prog.push(i);
        (self.prog.len() - 1) as u32
    }
    fn pc(&self) -> u32 {
        self.prog.len() as u32
    }
    fn set(&mut self, s: &CharSet) -> u32 {
        if let Some(i) = self.sets.iter().position(|x| x == s) {
            return i as u32;
        }
        self.sets.push(s.clone());
        (self.sets.len() - 1) as u32
    }

    fn item_of(&mut self, n: &Node) -> Option<Item> {
        Some(match n {
            Node::Lit(c, f) => Item::Lit(*c, *f),
            Node::Set(s, f) => Item::Set(self.set(s), *f),
            Node::Any => Item::Any,
            Node::AnyNoNl { unix } => Item::AnyNoNl { unix: *unix },
            Node::Group(inner, None) => return self.item_of(inner),
            _ => return None,
        })
    }

    fn node(&mut self, n: &Node) {
        match n {
            Node::Empty => {}
            Node::Lit(..) | Node::Set(..) | Node::Any | Node::AnyNoNl { .. } => {
                let it = self.item_of(n).unwrap();
                self.emit(Inst::Item(it));
            }
            Node::Anchor(a) => {
                self.emit(Inst::Anchor(*a));
            }
            Node::LineBreak => {
                self.emit(Inst::LineBreak);
            }
            Node::Group(inner, idx) => match idx {
                Some(i) => {
                    self.emit(Inst::Save((2 * i) as u32));
                    self.node(inner);
                    self.emit(Inst::Save((2 * i + 1) as u32));
                }
                None => self.node(inner),
            },
            Node::Concat(v) => {
                for x in v {
                    self.node(x);
                }
            }
            Node::Alt(v) => {
                let mut jumps = Vec::new();
                for (k, x) in v.iter().enumerate() {
                    if k + 1 < v.len() {
                        let split = self.emit(Inst::Split(0, 0));
                        self.node(x);
                        jumps.push(self.emit(Inst::Jmp(0)));
                        let next = self.pc();
                        self.prog[split as usize] = Inst::Split(split + 1, next);
                    } else {
                        self.node(x);
                    }
                }
                let end = self.pc();
                for j in jumps {
                    self.prog[j as usize] = Inst::Jmp(end);
                }
            }
            Node::Backref(g, f) => {
                self.emit(Inst::Backref(*g as u32, *f));
            }
            Node::Look { node, behind, neg } => {
                let bounds = if *behind {
                    let (lo, hi) = widths(node);
                    Some((lo as u32, hi.unwrap_or(0) as u32))
                } else {
                    None
                };
                let at = self.emit(Inst::Look { behind: bounds, neg: *neg, end: 0 });
                self.node(node);
                self.emit(Inst::SubMatch);
                let end = self.pc();
                self.prog[at as usize] = Inst::Look { behind: bounds, neg: *neg, end };
            }
            Node::Atomic(node) => {
                let at = self.emit(Inst::Atomic { end: 0 });
                self.node(node);
                self.emit(Inst::SubMatch);
                let end = self.pc();
                self.prog[at as usize] = Inst::Atomic { end };
            }
            Node::Repeat { node, min, max, kind, .. } => {
                if *kind == RepKind::Possessive {
                    let at = self.emit(Inst::Atomic { end: 0 });
                    self.repeat(node, *min, *max, true);
                    self.emit(Inst::SubMatch);
                    let end = self.pc();
                    self.prog[at as usize] = Inst::Atomic { end };
                } else {
                    self.repeat(node, *min, *max, *kind == RepKind::Greedy);
                }
            }
        }
    }

    fn repeat(&mut self, node: &Node, min: u32, max: u32, greedy: bool) {
        if let Some(it) = self.item_of(node) {
            self.emit(Inst::Repeat { item: it, min, max, greedy });
            return;
        }
        if max == 1 && min <= 1 {
            if min == 1 {
                self.node(node);
                return;
            }
            let split = self.emit(Inst::Split(0, 0));
            self.node(node);
            let end = self.pc();
            self.prog[split as usize] = if greedy { Inst::Split(split + 1, end) } else { Inst::Split(end, split + 1) };
            return;
        }
        let reg = self.nregs;
        let pos = self.nregs + 1;
        self.nregs += 2;
        self.emit(Inst::LoopInit { reg });
        let head = self.emit(Inst::Loop { reg, pos, min, max, greedy, exit: 0 });
        self.node(node);
        self.emit(Inst::LoopEnd { head });
        let exit = self.pc();
        self.prog[head as usize] = Inst::Loop { reg, pos, min, max, greedy, exit };
    }
}

impl Regex {
    pub fn compile(p: &str, flags: u32) -> Result<Regex, PatternError> {
        // `Pattern`'s constructor: Unicode character classes imply Unicode case folding
        let flags = if flags & UNICODE_CHARACTER_CLASS != 0 { flags | UNICODE_CASE } else { flags };
        let chars: Vec<char> = p.chars().collect();
        let mut ps = Parser { p: &chars, i: 0, flags, groups: 0, names: Vec::new(), warnings: Vec::new() };
        let node = if flags & LITERAL != 0 {
            let fold = ps.fold();
            Node::Concat(chars.iter().map(|&c| Node::Lit(c as u32, fold)).collect())
        } else {
            ps.parse()?
        };
        let mut c = Compiler { prog: Vec::new(), sets: Vec::new(), nregs: 0 };
        c.emit(Inst::Save(0));
        c.node(&node);
        c.emit(Inst::Save(1));
        c.emit(Inst::Match);
        Ok(Regex { source: p.to_string(), flags, prog: c.prog, sets: c.sets, ngroups: ps.groups, nregs: c.nregs as usize, names: ps.names, warnings: ps.warnings })
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn flags(&self) -> u32 {
        self.flags
    }
    pub fn group_count(&self) -> usize {
        self.ngroups
    }
    /// Warnings the [`ONIG`] parser emitted (`regular expression has redundant nested repeat
    /// operator ...`).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Named groups in definition order (names may repeat in [`ONIG`] mode).
    pub fn group_names(&self) -> &[(String, usize)] {
        &self.names
    }
    pub fn group_index(&self, name: &str) -> Option<usize> {
        self.names.iter().find(|(n, _)| n == name).map(|(_, i)| *i)
    }
}

// ------------------------------------------------------------------------------ VM

/// The regex-limit counter (`LimitedCharSequence`).
#[derive(Clone, Debug)]
pub struct Limit {
    pub factor: i32,
    pub limit: u64,
    pub count: u64,
}

/// Raised when the limit is hit; converted to the Elasticsearch error by the caller.
pub struct LimitHit;

enum Frame {
    Alt { pc: u32, pos: usize },
    Cap { slot: u32, old: usize },
    Reg { reg: u32, old: usize },
    RepGreedy { pc: u32, min_pos: usize, pos: usize },
    RepLazy { pc: u32, item_pc: u32, count: u32, max: u32, pos: usize },
    Barrier,
}

const NONE: usize = usize::MAX;

struct Exec<'a> {
    re: &'a Regex,
    t: &'a [u16],
    /// Matching bounds (region) and lookaround bounds (whole text with transparent bounds).
    from: usize,
    to: usize,
    look_from: usize,
    look_to: usize,
    anchoring: bool,
    last_match_end: usize,
    caps: Vec<usize>,
    regs: Vec<usize>,
    stack: Vec<Frame>,
    limit: Option<&'a mut Limit>,
    hit_end: bool,
}

#[inline]
fn is_line_term(c: u32, unix: bool) -> bool {
    if unix {
        c == 10
    } else {
        c == 10 || c == 13 || c == 0x85 || c == 0x2028 || c == 0x2029
    }
}

fn fold_ascii(c: u32) -> u32 {
    if (b'A' as u32..=b'Z' as u32).contains(&c) {
        c + 32
    } else {
        c
    }
}

fn to_upper(c: u32) -> u32 {
    char::from_u32(c).map(|ch| {
        let mut u = ch.to_uppercase();
        match (u.next(), u.next()) {
            (Some(x), None) => x as u32,
            _ => c,
        }
    })
    .unwrap_or(c)
}

fn to_lower(c: u32) -> u32 {
    char::from_u32(c).map(|ch| {
        let mut u = ch.to_lowercase();
        match (u.next(), u.next()) {
            (Some(x), None) => x as u32,
            _ => c,
        }
    })
    .unwrap_or(c)
}

fn chars_equal(a: u32, b: u32, f: Fold) -> bool {
    if a == b {
        return true;
    }
    match f {
        Fold::None => false,
        Fold::Ascii => a < 128 && b < 128 && fold_ascii(a) == fold_ascii(b),
        Fold::Unicode => to_upper(a) == to_upper(b) || to_lower(a) == to_lower(b),
    }
}

impl Exec<'_> {
    /// Code point at `pos` and its width; counts the reads against the limit.
    #[inline]
    fn cp(&mut self, pos: usize, end: usize) -> Result<Option<(u32, usize)>, LimitHit> {
        if pos >= end {
            self.hit_end = true;
            return Ok(None);
        }
        self.count(1)?;
        let c = self.t[pos] as u32;
        if (0xD800..0xDC00).contains(&c) && pos + 1 < end {
            let d = self.t[pos + 1] as u32;
            if (0xDC00..0xE000).contains(&d) {
                self.count(1)?;
                return Ok(Some((0x10000 + ((c - 0xD800) << 10) + (d - 0xDC00), 2)));
            }
        }
        Ok(Some((c, 1)))
    }

    #[inline]
    fn count(&mut self, n: u64) -> Result<(), LimitHit> {
        if let Some(l) = self.limit.as_deref_mut() {
            l.count += n;
            if l.count > l.limit {
                return Err(LimitHit);
            }
        }
        Ok(())
    }

    /// Code point just before `pos` (within `lo`) and its width.
    fn cp_before(&mut self, pos: usize, lo: usize) -> Result<Option<(u32, usize)>, LimitHit> {
        if pos <= lo {
            return Ok(None);
        }
        self.count(1)?;
        let c = self.t[pos - 1] as u32;
        if (0xDC00..0xE000).contains(&c) && pos >= lo + 2 {
            let h = self.t[pos - 2] as u32;
            if (0xD800..0xDC00).contains(&h) {
                self.count(1)?;
                return Ok(Some((0x10000 + ((h - 0xD800) << 10) + (c - 0xDC00), 2)));
            }
        }
        Ok(Some((c, 1)))
    }

    #[inline]
    fn item(&mut self, it: &Item, pos: usize) -> Result<Option<usize>, LimitHit> {
        let to = self.to;
        let Some((c, w)) = self.cp(pos, to)? else { return Ok(None) };
        let ok = match it {
            Item::Lit(x, f) => chars_equal(c, *x, *f),
            Item::Set(s, f) => {
                let set = &self.re.sets[*s as usize];
                set.contains(c)
                    || match f {
                        Fold::None => false,
                        Fold::Ascii => c < 128 && (set.contains(fold_ascii(c)) || (b'a' as u32..=b'z' as u32).contains(&c) && set.contains(c - 32)),
                        Fold::Unicode => set.contains(to_upper(c)) || set.contains(to_lower(c)),
                    }
            }
            Item::Any => true,
            Item::AnyNoNl { unix } => !is_line_term(c, *unix),
        };
        Ok(if ok { Some(pos + w) } else { None })
    }

    fn is_word(&self, c: u32, ucc: bool) -> bool {
        if ucc {
            char::from_u32(c).is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        } else {
            c < 128 && ((c as u8).is_ascii_alphanumeric() || c == b'_' as u32)
        }
    }

    fn anchor(&mut self, a: Anchor, pos: usize) -> Result<bool, LimitHit> {
        let (lo, hi) = if self.anchoring { (self.from, self.to) } else { (0, self.t.len()) };
        Ok(match a {
            Anchor::Begin => pos == lo,
            Anchor::End => {
                if pos == hi {
                    self.hit_end = true;
                    true
                } else {
                    false
                }
            }
            Anchor::LastMatch => pos == self.last_match_end,
            Anchor::Bol { multiline, unix } => {
                if pos == lo {
                    return Ok(true);
                }
                if !multiline {
                    return Ok(false);
                }
                if pos >= hi {
                    // `^` in MULTILINE mode never matches at the end of input
                    return Ok(false);
                }
                let prev = self.t[pos - 1] as u32;
                if unix {
                    prev == 10
                } else if prev == 13 {
                    // not between `\r` and `\n`
                    self.t[pos] != 10
                } else {
                    is_line_term(prev, false)
                }
            }
            Anchor::EndOptNl { unix } => self.end_opt_nl(pos, lo, hi, unix),
            Anchor::Eol { multiline: false, unix } => self.end_opt_nl(pos, lo, hi, unix),
            Anchor::Eol { multiline: true, unix } => {
                if pos >= hi {
                    self.hit_end = true;
                    return Ok(true);
                }
                let c = self.t[pos] as u32;
                if unix {
                    return Ok(c == 10);
                }
                if c == 10 && pos > lo && self.t[pos - 1] == 13 {
                    return Ok(false);
                }
                is_line_term(c, false)
            }
            Anchor::Word { neg, ucc } => {
                let (llo, lhi) = (self.look_from, self.look_to);
                let before = self.cp_before(pos, llo)?.map(|(c, _)| self.is_word(c, ucc)).unwrap_or(false);
                let after = self.cp(pos, lhi)?.map(|(c, _)| self.is_word(c, ucc)).unwrap_or(false);
                (before != after) != neg
            }
        })
    }

    /// `$` without MULTILINE and `\Z`: the end, or before a final line terminator.
    fn end_opt_nl(&mut self, pos: usize, lo: usize, hi: usize, unix: bool) -> bool {
        if pos == hi {
            self.hit_end = true;
            return true;
        }
        let rest = hi - pos;
        let c = self.t[pos] as u32;
        if unix {
            return rest == 1 && c == 10;
        }
        if rest == 2 && c == 13 && self.t[pos + 1] == 10 {
            return true;
        }
        if rest == 1 && is_line_term(c, false) {
            // never between `\r` and `\n`
            return !(c == 10 && pos > lo && self.t[pos - 1] == 13);
        }
        false
    }

    fn set_cap(&mut self, slot: u32, v: usize) {
        let old = self.caps[slot as usize];
        self.stack.push(Frame::Cap { slot, old });
        self.caps[slot as usize] = v;
    }

    fn set_reg(&mut self, reg: u32, v: usize) {
        let old = self.regs[reg as usize];
        self.stack.push(Frame::Reg { reg, old });
        self.regs[reg as usize] = v;
    }

    /// Runs from `pc` at `pos` until `Match`/`SubMatch`; `need_end` constrains where a match
    /// may end. Returns the end position, or `None` after unwinding to (and removing) the
    /// barrier the caller pushed.
    fn run(&mut self, mut pc: u32, mut pos: usize, need_end: Option<usize>) -> Result<Option<usize>, LimitHit> {
        let prog = &self.re.prog;
        loop {
            let ok: bool = match &prog[pc as usize] {
                Inst::Item(it) => match self.item(it, pos)? {
                    Some(p) => {
                        pos = p;
                        pc += 1;
                        true
                    }
                    None => false,
                },
                Inst::Anchor(a) => {
                    if self.anchor(*a, pos)? {
                        pc += 1;
                        true
                    } else {
                        false
                    }
                }
                Inst::LineBreak => {
                    let to = self.to;
                    match self.cp(pos, to)? {
                        Some((13, _)) if pos + 1 < to && self.t[pos + 1] == 10 => {
                            // `\R` prefers `\r\n` but may backtrack to `\r` alone
                            self.stack.push(Frame::Alt { pc: pc + 1, pos: pos + 1 });
                            pos += 2;
                            pc += 1;
                            true
                        }
                        Some((c, w)) if (10..=13).contains(&c) || c == 0x85 || c == 0x2028 || c == 0x2029 => {
                            pos += w;
                            pc += 1;
                            true
                        }
                        _ => false,
                    }
                }
                Inst::Split(a, b) => {
                    self.stack.push(Frame::Alt { pc: *b, pos });
                    pc = *a;
                    true
                }
                Inst::Jmp(t) => {
                    pc = *t;
                    true
                }
                Inst::Save(slot) => {
                    self.set_cap(*slot, pos);
                    pc += 1;
                    true
                }
                Inst::Backref(g, f) => {
                    let (s, e) = (self.caps.get(2 * *g as usize).copied().unwrap_or(NONE), self.caps.get(2 * *g as usize + 1).copied().unwrap_or(NONE));
                    if s == NONE || e == NONE {
                        false
                    } else {
                        let n = e - s;
                        if pos + n > self.to {
                            self.hit_end = true;
                            false
                        } else {
                            self.count(n as u64)?;
                            let mut eq = true;
                            for k in 0..n {
                                let (a, b) = (self.t[s + k] as u32, self.t[pos + k] as u32);
                                if !chars_equal(a, b, *f) {
                                    eq = false;
                                    break;
                                }
                            }
                            if eq {
                                pos += n;
                                pc += 1;
                            }
                            eq
                        }
                    }
                }
                Inst::Repeat { item, min, max, greedy } => {
                    let (min, max, greedy) = (*min, *max, *greedy);
                    let mut count = 0;
                    let mut ok = true;
                    while count < min {
                        match self.item(item, pos)? {
                            Some(p) => {
                                pos = p;
                                count += 1;
                            }
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        if greedy {
                            let min_pos = pos;
                            while count < max {
                                match self.item(item, pos)? {
                                    Some(p) => {
                                        pos = p;
                                        count += 1;
                                    }
                                    None => break,
                                }
                            }
                            if pos > min_pos {
                                self.stack.push(Frame::RepGreedy { pc: pc + 1, min_pos, pos });
                            }
                        } else if count < max {
                            self.stack.push(Frame::RepLazy { pc: pc + 1, item_pc: pc, count, max, pos });
                        }
                        pc += 1;
                    }
                    ok
                }
                Inst::LoopInit { reg } => {
                    self.set_reg(*reg, 0);
                    pc += 1;
                    true
                }
                Inst::Loop { reg, pos: preg, min, max, greedy, exit } => {
                    let count = self.regs[*reg as usize];
                    let (reg, preg, exit) = (*reg, *preg, *exit);
                    if (count as u64) < *min as u64 {
                        self.set_reg(reg, count + 1);
                        self.set_reg(preg, pos);
                        pc += 1;
                    } else if *max == UNBOUNDED || (count as u64) < *max as u64 {
                        if *greedy {
                            self.stack.push(Frame::Alt { pc: exit, pos });
                            self.set_reg(reg, count + 1);
                            self.set_reg(preg, pos);
                            pc += 1;
                        } else {
                            // lazy: exit first; backtracking enters one more iteration
                            self.stack.push(Frame::Alt { pc: pc | 0x8000_0000, pos });
                            pc = exit;
                        }
                    } else {
                        pc = exit;
                    }
                    true
                }
                Inst::LoopEnd { head } => {
                    let Inst::Loop { reg, pos: preg, min, exit, .. } = prog[*head as usize] else { unreachable!() };
                    let count = self.regs[reg as usize];
                    if pos == self.regs[preg as usize] && count as u64 >= min as u64 {
                        // an empty iteration ends the loop
                        pc = exit;
                    } else {
                        pc = *head;
                    }
                    true
                }
                Inst::Look { behind, neg, end } => {
                    let (behind, neg, end) = (*behind, *neg, *end);
                    let save = (self.from, self.to);
                    self.from = self.look_from;
                    self.to = self.look_to;
                    let base = self.stack.len();
                    let mut matched = false;
                    match behind {
                        None => {
                            self.stack.push(Frame::Barrier);
                            if self.run(pc + 1, pos, None)?.is_some() {
                                matched = true;
                            }
                        }
                        Some((lo, hi)) => {
                            let start_lo = pos.saturating_sub(hi as usize).max(self.look_from);
                            let mut j = pos as isize - lo as isize;
                            while j >= start_lo as isize {
                                self.stack.push(Frame::Barrier);
                                if self.run(pc + 1, j as usize, Some(pos))?.is_some() {
                                    matched = true;
                                    break;
                                }
                                j -= 1;
                            }
                        }
                    }
                    self.from = save.0;
                    self.to = save.1;
                    if matched {
                        if neg {
                            self.unwind_to(base);
                            false
                        } else {
                            self.cut(base);
                            pc = end;
                            true
                        }
                    } else if neg {
                        pc = end;
                        true
                    } else {
                        false
                    }
                }
                Inst::Atomic { end } => {
                    let end = *end;
                    let base = self.stack.len();
                    self.stack.push(Frame::Barrier);
                    match self.run(pc + 1, pos, None)? {
                        Some(p) => {
                            self.cut(base);
                            pos = p;
                            pc = end;
                            true
                        }
                        None => false,
                    }
                }
                Inst::SubMatch => {
                    if let Some(e) = need_end {
                        if pos != e {
                            false
                        } else {
                            return Ok(Some(pos));
                        }
                    } else {
                        return Ok(Some(pos));
                    }
                }
                Inst::Match => {
                    if let Some(e) = need_end {
                        if pos != e {
                            false
                        } else {
                            return Ok(Some(pos));
                        }
                    } else {
                        return Ok(Some(pos));
                    }
                }
            };
            if ok {
                continue;
            }
            // backtrack
            loop {
                let Some(f) = self.stack.pop() else { return Ok(None) };
                match f {
                    Frame::Alt { pc: p, pos: q } => {
                        if p & 0x8000_0000 != 0 {
                            // lazy loop: take one more iteration now
                            let head = p & 0x7FFF_FFFF;
                            let Inst::Loop { reg, pos: preg, .. } = prog[head as usize] else { unreachable!() };
                            let count = self.regs[reg as usize];
                            pos = q;
                            self.set_reg(reg, count + 1);
                            self.set_reg(preg, pos);
                            pc = head + 1;
                        } else {
                            pc = p;
                            pos = q;
                        }
                        break;
                    }
                    Frame::Cap { slot, old } => self.caps[slot as usize] = old,
                    Frame::Reg { reg, old } => self.regs[reg as usize] = old,
                    Frame::RepGreedy { pc: p, min_pos, pos: q } => {
                        // give back one code point
                        let lo = min_pos;
                        let w = match self.cp_before(q, lo)? {
                            Some((_, w)) => w,
                            None => 1,
                        };
                        let np = q - w;
                        if np > min_pos {
                            self.stack.push(Frame::RepGreedy { pc: p, min_pos, pos: np });
                        }
                        pc = p;
                        pos = np;
                        break;
                    }
                    Frame::RepLazy { pc: p, item_pc, count, max, pos: q } => {
                        let Inst::Repeat { item, .. } = &prog[item_pc as usize] else { unreachable!() };
                        let item = item.clone();
                        match self.item(&item, q)? {
                            Some(np) => {
                                if count + 1 < max {
                                    self.stack.push(Frame::RepLazy { pc: p, item_pc, count: count + 1, max, pos: np });
                                }
                                pc = p;
                                pos = np;
                                break;
                            }
                            None => continue,
                        }
                    }
                    Frame::Barrier => return Ok(None),
                }
            }
        }
    }

    /// Pops frames above `base`, restoring captures and registers.
    fn unwind_to(&mut self, base: usize) {
        while self.stack.len() > base {
            match self.stack.pop().unwrap() {
                Frame::Cap { slot, old } => self.caps[slot as usize] = old,
                Frame::Reg { reg, old } => self.regs[reg as usize] = old,
                _ => {}
            }
        }
    }

    /// Commits a nested match: drops its alternatives but keeps restore frames.
    fn cut(&mut self, base: usize) {
        let mut keep = Vec::new();
        while self.stack.len() > base {
            if let f @ (Frame::Cap { .. } | Frame::Reg { .. }) = self.stack.pop().unwrap() { keep.push(f) }
        }
        while let Some(f) = keep.pop() {
            self.stack.push(f);
        }
    }
}

/// How a match attempt is anchored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `find`: any start at or after `from`.
    Search,
    /// `lookingAt`: start at `from`.
    Prefix,
    /// `matches`: the whole region.
    Whole,
}

impl Regex {
    /// Attempts a match; fills `caps` (2 slots per group, `usize::MAX` when unset).
    #[allow(clippy::too_many_arguments)]
    fn exec(
        &self,
        t: &[u16],
        from: usize,
        to: usize,
        start: usize,
        mode: Mode,
        transparent: bool,
        anchoring: bool,
        last_match_end: usize,
        limit: Option<&mut Limit>,
        caps: &mut Vec<usize>,
    ) -> Result<(bool, bool), LimitHit> {
        let mut e = Exec {
            re: self,
            t,
            from,
            to,
            look_from: if transparent { 0 } else { from },
            look_to: if transparent { t.len() } else { to },
            anchoring,
            last_match_end,
            caps: vec![NONE; 2 * (self.ngroups + 1)],
            regs: vec![0; self.nregs],
            stack: Vec::new(),
            limit,
            hit_end: false,
        };
        let need_end = if mode == Mode::Whole { Some(to) } else { None };
        let mut s = start;
        loop {
            e.stack.clear();
            for c in e.caps.iter_mut() {
                *c = NONE;
            }
            if e.run(0, s, need_end)?.is_some() {
                *caps = std::mem::take(&mut e.caps);
                return Ok((true, e.hit_end));
            }
            if mode != Mode::Search || s >= to {
                return Ok((false, e.hit_end));
            }
            // advance one code point
            s += if (0xD800..0xDC00).contains(&t[s]) && s + 1 < to && (0xDC00..0xE000).contains(&t[s + 1]) { 2 } else { 1 };
        }
    }
}

// ------------------------------------------------------------------------------ Matcher

/// State of a `java.util.regex.Matcher`.
pub struct MatcherState {
    pub regex: Arc<Regex>,
    text: Rc<[u16]>,
    from: usize,
    to: usize,
    /// Start and end of the last match (`first == NONE` when there is none).
    first: usize,
    last: usize,
    groups: Vec<usize>,
    append_pos: usize,
    hit_end: bool,
    anchoring: bool,
    transparent: bool,
    limit: Option<Limit>,
}

fn utf16(s: &str) -> Rc<[u16]> {
    if s.is_ascii() {
        s.bytes().map(|b| b as u16).collect()
    } else {
        s.encode_utf16().collect()
    }
}

fn limit_error(re: &Regex, text: &[u16], l: &Limit, whole: bool) -> Thrown {
    let len = text.len();
    let snippet = if len <= 64 { String::from_utf16_lossy(text) } else { format!("{}...", String::from_utf16_lossy(&text[..61])) };
    let msg = format!(
        "[scripting] Regular expression considered too many characters, pattern: [{}], limit factor: [{}], char limit: [{}], wrapped: [{}], this limit can be changed by the [script.painless.regex.limit-factor] setting",
        re.source, l.factor, l.limit, snippet
    );
    let mut t = Throw::new(cls::RUNTIME_EXCEPTION, Some(msg));
    t.es_type = Some("circuit_breaking_exception");
    t.es_root = true;
    t.cause_fields = vec![
        ("bytes_wanted".into(), serde_json::json!(0)),
        ("bytes_limit".into(), serde_json::json!(0)),
        ("durability".into(), serde_json::json!("TRANSIENT")),
    ];
    // the innermost frames are java.util.regex internals that depend on the pattern; the
    // reader and the Matcher entry point bracket them
    let entry: &[&str] = if whole {
        &["java.base/java.util.regex.Matcher.match(Matcher.java:1792)", "java.base/java.util.regex.Matcher.matches(Matcher.java:752)"]
    } else {
        &["java.base/java.util.regex.Matcher.search(Matcher.java:1767)", "java.base/java.util.regex.Matcher.find(Matcher.java:787)"]
    };
    let mut frames = vec!["org.elasticsearch.server@9.5.5/org.elasticsearch.common.text.ReadLimitedCharSequence.charAt(ReadLimitedCharSequence.java:57)".to_string()];
    frames.extend(entry.iter().map(|s| s.to_string()));
    t.frames = frames;
    t
}

impl MatcherState {
    pub fn new(regex: Arc<Regex>, input: &str, limit_factor: i32) -> MatcherState {
        let text = utf16(input);
        let len = text.len();
        let limit = if limit_factor > 0 { Some(Limit { factor: limit_factor, limit: limit_factor as u64 * len as u64, count: 0 }) } else { None };
        let groups = vec![NONE; 2 * (regex.ngroups + 1)];
        MatcherState { regex, text, from: 0, to: len, first: NONE, last: 0, groups, append_pos: 0, hit_end: false, anchoring: true, transparent: false, limit }
    }

    fn run(&mut self, start: usize, mode: Mode) -> Result<bool, Thrown> {
        let mut caps = Vec::new();
        let last_end = if self.first == NONE { self.from } else { self.last };
        let r = self.regex.exec(&self.text, self.from, self.to, start, mode, self.transparent, self.anchoring, last_end, self.limit.as_mut(), &mut caps);
        match r {
            Ok((true, hit)) => {
                self.hit_end = hit;
                self.first = caps[0];
                self.last = caps[1];
                self.groups = caps;
                Ok(true)
            }
            Ok((false, hit)) => {
                self.hit_end = hit;
                self.first = NONE;
                for g in self.groups.iter_mut() {
                    *g = NONE;
                }
                Ok(false)
            }
            Err(LimitHit) => Err(limit_error(&self.regex, &self.text, self.limit.as_ref().unwrap(), mode != Mode::Search)),
        }
    }

    pub fn find(&mut self) -> Result<bool, Thrown> {
        let mut next = self.last;
        if self.first != NONE && next == self.first {
            next += 1;
        }
        if next < self.from {
            next = self.from;
        }
        if next > self.to {
            for g in self.groups.iter_mut() {
                *g = NONE;
            }
            self.first = NONE;
            return Ok(false);
        }
        self.run(next, Mode::Search)
    }

    pub fn find_from(&mut self, start: i32) -> Result<bool, Thrown> {
        let limit = self.text.len() as i32;
        if start < 0 || start > limit {
            return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some("Illegal start index".into())));
        }
        self.reset();
        self.run(start as usize, Mode::Search)
    }

    pub fn matches(&mut self) -> Result<bool, Thrown> {
        let f = self.from;
        self.run(f, Mode::Whole)
    }

    pub fn looking_at(&mut self) -> Result<bool, Thrown> {
        let f = self.from;
        self.run(f, Mode::Prefix)
    }

    pub fn reset(&mut self) {
        self.first = NONE;
        self.last = 0;
        self.from = 0;
        self.to = self.text.len();
        self.append_pos = 0;
        for g in self.groups.iter_mut() {
            *g = NONE;
        }
    }

    /// `checkMatch`, called from `method` (frames of JDK 27's Matcher.java).
    fn need_match(&self, via: &[&str]) -> Result<(), Thrown> {
        if self.first == NONE {
            let mut f = vec!["java.base/java.util.regex.Matcher.checkMatch(Matcher.java:1850)"];
            f.extend_from_slice(via);
            return Err(Throw::new(cls::ILLEGAL_STATE_EXCEPTION, Some("No match found".into())).with_frames(&f));
        }
        Ok(())
    }

    fn check_group(&self, g: i32, via: &[&str]) -> Result<(), Thrown> {
        if g < 0 || g as usize > self.regex.ngroups {
            let mut f = vec!["java.base/java.util.regex.Matcher.checkGroup(Matcher.java:1845)"];
            f.extend_from_slice(via);
            return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("No group {g}"))).with_frames(&f));
        }
        Ok(())
    }

    /// `group()` (`whole`) or `group(int)`.
    pub fn group_at(&self, g: i32, whole: bool) -> Result<Option<String>, Thrown> {
        let via: &[&str] = if whole {
            &["java.base/java.util.regex.Matcher.group(Matcher.java:685)", "java.base/java.util.regex.Matcher.group(Matcher.java:643)"]
        } else {
            &["java.base/java.util.regex.Matcher.group(Matcher.java:685)"]
        };
        self.need_match(via)?;
        self.check_group(g, &["java.base/java.util.regex.Matcher.group(Matcher.java:686)"])?;
        let (s, e) = (self.groups[2 * g as usize], self.groups[2 * g as usize + 1]);
        if s == NONE || e == NONE {
            return Ok(None);
        }
        Ok(Some(String::from_utf16_lossy(&self.text[s..e])))
    }

    pub fn group(&self, g: i32) -> Result<Option<String>, Thrown> {
        self.need_match(&[])?;
        self.check_group(g, &[])?;
        let (s, e) = (self.groups[2 * g as usize], self.groups[2 * g as usize + 1]);
        if s == NONE || e == NONE {
            return Ok(None);
        }
        Ok(Some(String::from_utf16_lossy(&self.text[s..e])))
    }

    /// `start()` (`whole`) or `start(int)`.
    pub fn start_at(&self, g: i32, whole: bool) -> Result<i32, Thrown> {
        self.need_match(&[if whole { "java.base/java.util.regex.Matcher.start(Matcher.java:493)" } else { "java.base/java.util.regex.Matcher.start(Matcher.java:522)" }])?;
        self.check_group(g, &["java.base/java.util.regex.Matcher.start(Matcher.java:523)"])?;
        let s = self.groups[2 * g as usize];
        Ok(if s == NONE { -1 } else { s as i32 })
    }

    /// `end()` (`whole`) or `end(int)`.
    pub fn end_at(&self, g: i32, whole: bool) -> Result<i32, Thrown> {
        self.need_match(&[if whole { "java.base/java.util.regex.Matcher.end(Matcher.java:562)" } else { "java.base/java.util.regex.Matcher.end(Matcher.java:591)" }])?;
        self.check_group(g, &["java.base/java.util.regex.Matcher.end(Matcher.java:592)"])?;
        let e = self.groups[2 * g as usize + 1];
        Ok(if e == NONE { -1 } else { e as i32 })
    }

    pub fn start(&self, g: i32) -> Result<i32, Thrown> {
        self.need_match(&[])?;
        self.check_group(g, &[])?;
        let s = self.groups[2 * g as usize];
        Ok(if s == NONE { -1 } else { s as i32 })
    }

    pub fn end(&self, g: i32) -> Result<i32, Thrown> {
        self.need_match(&[])?;
        self.check_group(g, &[])?;
        let e = self.groups[2 * g as usize + 1];
        Ok(if e == NONE { -1 } else { e as i32 })
    }

    /// `appendReplacement` with Java's replacement syntax (`$1`, `${name}`, `\$`).
    /// Errors carry the frames of JDK 27's `appendExpandedReplacement` (its throwing line) and
    /// `appendReplacement`; callers add their own.
    fn append_replacement(&mut self, out: &mut Vec<u16>, rep: &str) -> Result<(), Thrown> {
        self.need_match(&["java.base/java.util.regex.Matcher.appendReplacement(Matcher.java:1035)"])?;
        let mut buf: Vec<u16> = Vec::new();
        let r: Vec<char> = rep.chars().collect();
        let mut i = 0;
        let at = |line: u32| {
            [format!("java.base/java.util.regex.Matcher.appendExpandedReplacement(Matcher.java:{line})"), "java.base/java.util.regex.Matcher.appendReplacement(Matcher.java:1041)".to_string()]
        };
        let iae_at = |m: &str, line: u32| {
            let f = at(line);
            Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(m.to_string())).with_frames(&[f[0].as_str(), f[1].as_str()])
        };
        while i < r.len() {
            let c = r[i];
            if c == '\\' {
                i += 1;
                if i >= r.len() {
                    return Err(iae_at("character to be escaped is missing", 1063));
                }
                let mut tmp = [0u16; 2];
                buf.extend_from_slice(r[i].encode_utf16(&mut tmp));
                i += 1;
            } else if c == '$' {
                i += 1;
                if i >= r.len() {
                    return Err(iae_at("Illegal group reference: group index is missing", 1073));
                }
                let g: usize;
                if r[i] == '{' {
                    i += 1;
                    let s = i;
                    while i < r.len() && r[i].is_ascii_alphanumeric() {
                        i += 1;
                    }
                    if i == s {
                        return Err(iae_at("named capturing group has 0 length name", 1091));
                    }
                    if i >= r.len() || r[i] != '}' {
                        return Err(iae_at("named capturing group is missing trailing '}'", 1094));
                    }
                    let name: String = r[s..i].iter().collect();
                    i += 1;
                    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                        return Err(iae_at(&format!("capturing group name {{{name}}} starts with digit character"), 1098));
                    }
                    g = match self.regex.group_index(&name) {
                        Some(g) => g,
                        None => return Err(iae_at(&format!("No group with name {{{name}}}"), 1103)),
                    };
                } else {
                    let Some(d) = r[i].to_digit(10) else {
                        return Err(iae_at("Illegal group reference", 1111));
                    };
                    let mut n = d as usize;
                    i += 1;
                    if n > self.regex.ngroups {
                        let f = at(1134);
                        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("No group {n}"))).with_frames(&[
                            "java.base/java.util.regex.Matcher.checkGroup(Matcher.java:1845)",
                            "java.base/java.util.regex.Matcher.start(Matcher.java:523)",
                            f[0].as_str(),
                            f[1].as_str(),
                        ]));
                    }
                    while i < r.len() {
                        let Some(d) = r[i].to_digit(10) else { break };
                        let nn = n * 10 + d as usize;
                        if nn > self.regex.ngroups {
                            break;
                        }
                        n = nn;
                        i += 1;
                    }
                    g = n;
                }
                let (s, e) = (self.groups[2 * g], self.groups[2 * g + 1]);
                if s != NONE && e != NONE {
                    buf.extend_from_slice(&self.text[s..e]);
                }
            } else {
                let mut tmp = [0u16; 2];
                buf.extend_from_slice(c.encode_utf16(&mut tmp));
                i += 1;
            }
        }
        out.extend_from_slice(&self.text[self.append_pos..self.first]);
        out.extend_from_slice(&buf);
        self.append_pos = self.last;
        Ok(())
    }

    fn append_tail(&self, out: &mut Vec<u16>) {
        out.extend_from_slice(&self.text[self.append_pos..]);
    }

    pub fn replace(&mut self, rep: &str, all: bool) -> Result<String, Thrown> {
        self.reset();
        if !self.find()? {
            return Ok(String::from_utf16_lossy(&self.text));
        }
        let mut out = Vec::with_capacity(self.text.len() + 16);
        let caller = if all { "java.base/java.util.regex.Matcher.replaceAll(Matcher.java:1227)" } else { "java.base/java.util.regex.Matcher.replaceFirst(Matcher.java:1444)" };
        loop {
            self.append_replacement(&mut out, rep).map_err(|mut e| {
                e.frames.push(caller.to_string());
                e
            })?;
            if !all || !self.find()? {
                break;
            }
        }
        self.append_tail(&mut out);
        Ok(String::from_utf16_lossy(&out))
    }

    pub fn to_java_string(&self) -> String {
        let last = if self.first == NONE { String::new() } else { String::from_utf16_lossy(&self.text[self.first..self.last]) };
        format!("java.util.regex.Matcher[pattern={} region={},{} lastmatch={}]", self.regex.source, self.from, self.to, last)
    }
}

// ------------------------------------------------------------------------------ natives

fn pattern_of(v: &Value) -> Result<Arc<Regex>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Pattern(p) => Ok(p.clone()),
            _ => Err(crate::vm::cce("not a Pattern")),
        },
        Value::Null => Err(crate::vm::npe("Cannot invoke \"java.util.regex.Pattern.matcher(java.lang.CharSequence)\" because \"receiver\" is null")),
        _ => Err(crate::vm::cce("not a Pattern")),
    }
}

fn matcher_of(v: &Value) -> Result<&RefCell<MatcherState>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Matcher(m) => Ok(m),
            _ => Err(crate::vm::cce("not a Matcher")),
        },
        _ => Err(crate::vm::cce("not a Matcher")),
    }
}

fn input(v: &Value) -> Result<String, Thrown> {
    match v {
        Value::Null => Err(crate::vm::npe("Cannot invoke \"java.lang.CharSequence.length()\" because \"wrap\" is null")),
        other => crate::lib_lang::char_seq(other),
    }
}

/// `x =~ /p/` (find) and `x ==~ /p/` (matches).
pub fn regex_op(vm: &mut Vm, find: bool, s: &Value, p: &Value) -> Result<bool, Thrown> {
    let re = pattern_of(p)?;
    let text = input(s)?;
    let mut m = MatcherState::new(re, &text, vm.prog.regex_limit_factor);
    if find {
        m.find()
    } else {
        m.matches()
    }
}

fn new_matcher(vm: &Vm, re: Arc<Regex>, text: &str) -> Value {
    Value::obj(Obj::Matcher(RefCell::new(MatcherState::new(re, text, vm.prog.regex_limit_factor))))
}

fn p_matcher(vm: &mut Vm, a: &mut [Value]) -> VR {
    let re = pattern_of(&a[0])?;
    let text = input(&a[1])?;
    Ok(new_matcher(vm, re, &text))
}

fn p_pattern(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::str(pattern_of(&a[0])?.source()))
}

fn p_flags(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(pattern_of(&a[0])?.flags() as i32))
}

fn p_quote(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = crate::lib_lang::string(&a[0], "indexOf(String)")?.as_str().to_string();
    Ok(Value::string(quote(&s)))
}

/// `Pattern.quote`.
pub fn quote(s: &str) -> String {
    if !s.contains("\\E") {
        return format!("\\Q{s}\\E");
    }
    let mut out = String::from("\\Q");
    let mut cur = 0;
    while let Some(i) = s[cur..].find("\\E") {
        out.push_str(&s[cur..cur + i]);
        out.push_str("\\E\\\\E\\Q");
        cur += i + 2;
    }
    out.push_str(&s[cur..]);
    out.push_str("\\E");
    out
}

fn split_impl(vm: &mut Vm, re: Arc<Regex>, text: &str, limit: i32) -> Result<Vec<String>, Thrown> {
    let mut m = MatcherState::new(re, text, vm.prog.regex_limit_factor);
    let t = m.text.clone();
    let mut index = 0usize;
    let limited = limit > 0;
    let mut list: Vec<String> = Vec::new();
    while m.find()? {
        let (ms, me) = (m.first, m.last);
        if !limited || (list.len() as i32) < limit - 1 {
            if index == 0 && index == ms && ms == me {
                continue;
            }
            list.push(String::from_utf16_lossy(&t[index..ms]));
            index = me;
        } else if list.len() as i32 == limit - 1 {
            list.push(String::from_utf16_lossy(&t[index..]));
            index = me;
        }
    }
    if index == 0 {
        return Ok(vec![text.to_string()]);
    }
    if !limited || (list.len() as i32) < limit {
        list.push(String::from_utf16_lossy(&t[index..]));
    }
    if limit == 0 {
        while list.last().is_some_and(|s| s.is_empty()) {
            list.pop();
        }
    }
    Ok(list)
}

fn string_array(items: Vec<String>) -> Value {
    let vals: Vec<Value> = items.into_iter().map(Value::string).collect();
    crate::dynamic::array_from(crate::types::Ty::array(cls::STRING, 1), vals)
}

fn p_split(vm: &mut Vm, a: &mut [Value]) -> VR {
    let re = pattern_of(&a[0])?;
    let text = input(&a[1])?;
    Ok(string_array(split_impl(vm, re, &text, 0)?))
}

fn p_split2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let re = pattern_of(&a[0])?;
    let text = input(&a[1])?;
    let limit = crate::lib_lang::int(&a[2]);
    Ok(string_array(split_impl(vm, re, &text, limit)?))
}

fn p_split_as_stream(vm: &mut Vm, a: &mut [Value]) -> VR {
    let re = pattern_of(&a[0])?;
    let text = input(&a[1])?;
    // splitAsStream keeps trailing empty strings except a single trailing one (Java's
    // MatcherIterator): emulate with limit 0 semantics for the common cases
    let parts = split_impl(vm, re, &text, 0)?;
    let items = if text.is_empty() { vec![Value::str("")] } else { parts.into_iter().map(Value::string).collect() };
    Ok(crate::stream::unsized_stream_of(items))
}

fn p_as_predicate(_: &mut Vm, a: &mut [Value]) -> VR {
    let re = pattern_of(&a[0])?;
    Ok(crate::func::native(Some(cls::PREDICATE), move |vm, args| {
        let text = input(args.first().unwrap_or(&Value::Null))?;
        let mut m = MatcherState::new(re.clone(), &text, vm.prog.regex_limit_factor);
        Ok(Value::Bool(m.find()?))
    }))
}

fn m_find(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(matcher_of(&a[0])?.borrow_mut().find()?))
}
fn m_find_from(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = crate::lib_lang::int(&a[1]);
    Ok(Value::Bool(matcher_of(&a[0])?.borrow_mut().find_from(i)?))
}
fn m_matches(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(matcher_of(&a[0])?.borrow_mut().matches()?))
}
fn m_looking_at(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(matcher_of(&a[0])?.borrow_mut().looking_at()?))
}
fn opt_str(s: Option<String>) -> Value {
    match s {
        Some(s) => Value::string(s),
        None => Value::Null,
    }
}
fn m_group(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(opt_str(matcher_of(&a[0])?.borrow().group_at(0, true)?))
}
fn m_group_n(_: &mut Vm, a: &mut [Value]) -> VR {
    let g = crate::lib_lang::int(&a[1]);
    Ok(opt_str(matcher_of(&a[0])?.borrow().group_at(g, false)?))
}
fn m_named_group(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = matcher_of(&a[0])?.borrow();
    let name = match &a[1] {
        Value::Null => return Err(crate::vm::npe("Group name")),
        v => v.to_java_string(),
    };
    // `getMatchedGroupIndex`: the match is checked before the name
    m.need_match(&["java.base/java.util.regex.Matcher.getMatchedGroupIndex(Matcher.java:1836)", "java.base/java.util.regex.Matcher.group(Matcher.java:720)"])?;
    let Some(g) = m.regex.group_index(&name) else {
        return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(format!("No group with name <{name}>")))
            .with_frames(&["java.base/java.util.regex.Matcher.getMatchedGroupIndex(Matcher.java:1839)", "java.base/java.util.regex.Matcher.group(Matcher.java:720)"]));
    };
    Ok(opt_str(m.group(g as i32)?))
}
fn m_group_count(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(matcher_of(&a[0])?.borrow().regex.ngroups as i32))
}
fn m_start(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(matcher_of(&a[0])?.borrow().start_at(0, true)?))
}
fn m_start_n(_: &mut Vm, a: &mut [Value]) -> VR {
    let g = crate::lib_lang::int(&a[1]);
    Ok(Value::Int(matcher_of(&a[0])?.borrow().start_at(g, false)?))
}
fn m_end(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(matcher_of(&a[0])?.borrow().end_at(0, true)?))
}
fn m_end_n(_: &mut Vm, a: &mut [Value]) -> VR {
    let g = crate::lib_lang::int(&a[1]);
    Ok(Value::Int(matcher_of(&a[0])?.borrow().end_at(g, false)?))
}
fn m_hit_end(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(matcher_of(&a[0])?.borrow().hit_end))
}
fn m_require_end(_: &mut Vm, a: &mut [Value]) -> VR {
    let _ = matcher_of(&a[0])?;
    Ok(Value::Bool(false))
}
fn m_has_anchoring(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(matcher_of(&a[0])?.borrow().anchoring))
}
fn m_has_transparent(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(matcher_of(&a[0])?.borrow().transparent))
}
fn m_use_anchoring(_: &mut Vm, a: &mut [Value]) -> VR {
    matcher_of(&a[0])?.borrow_mut().anchoring = crate::lib_lang::boolean(&a[1]);
    Ok(a[0].clone())
}
fn m_use_transparent(_: &mut Vm, a: &mut [Value]) -> VR {
    matcher_of(&a[0])?.borrow_mut().transparent = crate::lib_lang::boolean(&a[1]);
    Ok(a[0].clone())
}
fn m_pattern(_: &mut Vm, a: &mut [Value]) -> VR {
    let re = matcher_of(&a[0])?.borrow().regex.clone();
    Ok(Value::obj(Obj::Pattern(re)))
}
fn m_use_pattern(_: &mut Vm, a: &mut [Value]) -> VR {
    let re = match &a[1] {
        Value::Null => return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some("Pattern cannot be null".into()))),
        v => pattern_of(v)?,
    };
    {
        let mut m = matcher_of(&a[0])?.borrow_mut();
        m.groups = vec![NONE; 2 * (re.ngroups + 1)];
        m.regex = re;
    }
    Ok(a[0].clone())
}
fn m_region(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, e) = (crate::lib_lang::int(&a[1]), crate::lib_lang::int(&a[2]));
    {
        let mut m = matcher_of(&a[0])?.borrow_mut();
        let len = m.text.len() as i32;
        if s < 0 || s > len {
            return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some("start".into())));
        }
        if e < 0 || e > len {
            return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some("end".into())));
        }
        if s > e {
            return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some("start > end".into())));
        }
        m.reset();
        m.from = s as usize;
        m.to = e as usize;
    }
    Ok(a[0].clone())
}
fn m_region_start(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(matcher_of(&a[0])?.borrow().from as i32))
}
fn m_region_end(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(matcher_of(&a[0])?.borrow().to as i32))
}
fn m_reset(_: &mut Vm, a: &mut [Value]) -> VR {
    matcher_of(&a[0])?.borrow_mut().reset();
    Ok(a[0].clone())
}
fn rep_arg(v: &Value) -> Result<String, Thrown> {
    match v {
        Value::Null => Err(crate::vm::npe("replacement")),
        v => Ok(v.to_java_string()),
    }
}
fn m_replace_all(_: &mut Vm, a: &mut [Value]) -> VR {
    let rep = rep_arg(&a[1])?;
    Ok(Value::string(matcher_of(&a[0])?.borrow_mut().replace(&rep, true)?))
}
fn m_replace_first(_: &mut Vm, a: &mut [Value]) -> VR {
    let rep = rep_arg(&a[1])?;
    Ok(Value::string(matcher_of(&a[0])?.borrow_mut().replace(&rep, false)?))
}
fn m_quote_replacement(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = crate::lib_lang::string(&a[0], "indexOf(int)")?.as_str().to_string();
    Ok(Value::string(quote_replacement(&s)))
}

/// `Matcher.quoteReplacement`.
pub fn quote_replacement(s: &str) -> String {
    if !s.contains(['\\', '$']) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c == '\\' || c == '$' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `CharSequence.replaceAll(Pattern, Function)` / `replaceFirst`: the function builds each
/// replacement from the matcher (no limit factor, as in Elasticsearch).
fn cs_replace(vm: &mut Vm, a: &mut [Value], all: bool) -> VR {
    let text = crate::lib_lang::char_seq(&a[0])?;
    let re = pattern_of(&a[1])?;
    let f = a[2].clone();
    let state = MatcherState::new(re, &text, 0);
    let mv = Value::obj(Obj::Matcher(RefCell::new(state)));
    let m = matcher_of(&mv)?;
    if !m.borrow_mut().find()? {
        return Ok(Value::string(text));
    }
    let mut out: Vec<u16> = Vec::with_capacity(text.len() + 16);
    loop {
        let r = crate::func::call(vm, &f, std::slice::from_ref(&mv))?;
        let rs = match r {
            Value::Null => "null".to_string(),
            v => v.to_java_string(),
        };
        m.borrow_mut().append_replacement(&mut out, &quote_replacement(&rs))?;
        if !all || !m.borrow_mut().find()? {
            break;
        }
    }
    m.borrow().append_tail(&mut out);
    Ok(Value::string(String::from_utf16_lossy(&out)))
}

pub fn cs_replace_all(vm: &mut Vm, a: &mut [Value]) -> VR {
    cs_replace(vm, a, true)
}

pub fn cs_replace_first(vm: &mut Vm, a: &mut [Value]) -> VR {
    cs_replace(vm, a, false)
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(PATTERN, &[OBJECT]);
    b.supers(MATCHER, &[OBJECT]);
    b.m(PATTERN, "Predicate asPredicate()", p_as_predicate);
    b.m(PATTERN, "int flags()", p_flags);
    let m = b.m(PATTERN, "Matcher matcher(CharSequence)", p_matcher);
    b.aug(m, "java.util.regex.Pattern.matcher(java.lang.CharSequence)");
    b.m(PATTERN, "String pattern()", p_pattern);
    b.s(PATTERN, "String quote(String)", p_quote);
    let m = b.m(PATTERN, "String[] split(CharSequence)", p_split);
    b.aug(m, "java.util.regex.Pattern.split(java.lang.CharSequence)");
    let m = b.m(PATTERN, "String[] split(CharSequence,int)", p_split2);
    b.aug(m, "java.util.regex.Pattern.split(java.lang.CharSequence, int)");
    let m = b.m(PATTERN, "Stream splitAsStream(CharSequence)", p_split_as_stream);
    b.aug(m, "java.util.regex.Pattern.splitAsStream(java.lang.CharSequence)");

    b.m(MATCHER, "int end()", m_end);
    b.m(MATCHER, "int end(int)", m_end_n);
    b.m(MATCHER, "boolean find()", m_find);
    b.m(MATCHER, "boolean find(int)", m_find_from);
    b.m(MATCHER, "String group()", m_group);
    b.m(MATCHER, "String group(int)", m_group_n);
    let m = b.m(MATCHER, "String namedGroup(String)", m_named_group);
    b.aug(m, "java.util.regex.Matcher.group(String)");
    b.m(MATCHER, "int groupCount()", m_group_count);
    b.m(MATCHER, "boolean hasAnchoringBounds()", m_has_anchoring);
    b.m(MATCHER, "boolean hasTransparentBounds()", m_has_transparent);
    b.m(MATCHER, "boolean hitEnd()", m_hit_end);
    b.m(MATCHER, "boolean lookingAt()", m_looking_at);
    b.m(MATCHER, "boolean matches()", m_matches);
    b.m(MATCHER, "Pattern pattern()", m_pattern);
    b.s(MATCHER, "String quoteReplacement(String)", m_quote_replacement);
    b.m(MATCHER, "Matcher region(int,int)", m_region);
    b.m(MATCHER, "int regionEnd()", m_region_end);
    b.m(MATCHER, "int regionStart()", m_region_start);
    b.m(MATCHER, "String replaceAll(String)", m_replace_all);
    b.m(MATCHER, "String replaceFirst(String)", m_replace_first);
    b.m(MATCHER, "boolean requireEnd()", m_require_end);
    b.m(MATCHER, "Matcher reset()", m_reset);
    b.m(MATCHER, "int start()", m_start);
    b.m(MATCHER, "int start(int)", m_start_n);
    b.m(MATCHER, "Matcher useAnchoringBounds(boolean)", m_use_anchoring);
    b.m(MATCHER, "Matcher usePattern(Pattern)", m_use_pattern);
    b.m(MATCHER, "Matcher useTransparentBounds(boolean)", m_use_transparent);
    let _ = JList::array_list;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_all(p: &str, flags: u32, s: &str) -> Vec<(i32, i32)> {
        let re = Arc::new(Regex::compile(p, flags).unwrap());
        let mut m = MatcherState::new(re, s, 0);
        let mut out = Vec::new();
        while m.find().unwrap() {
            out.push((m.start(0).unwrap(), m.end(0).unwrap()));
        }
        out
    }

    fn matches(p: &str, s: &str) -> bool {
        let re = Arc::new(Regex::compile(p, 0).unwrap());
        MatcherState::new(re, s, 0).matches().unwrap()
    }

    fn groups(p: &str, s: &str) -> Vec<Option<String>> {
        let re = Arc::new(Regex::compile(p, 0).unwrap());
        let n = re.ngroups;
        let mut m = MatcherState::new(re, s, 0);
        assert!(m.find().unwrap());
        (0..=n).map(|g| m.group(g as i32).unwrap()).collect()
    }

    fn perr(p: &str) -> (i32, String) {
        let e = Regex::compile(p, 0).unwrap_err();
        (e.index, e.description)
    }

    #[test]
    fn basics() {
        assert_eq!(find_all("a+", 0, "caaab aa"), vec![(1, 4), (6, 8)]);
        assert_eq!(find_all("a*", 0, "baa"), vec![(0, 0), (1, 3), (3, 3)]);
        assert!(matches("a|ab", "ab"));
        assert!(matches("(a+)+b", "aaab"));
        assert!(!matches("a.c", "a\nc"));
        assert!(matches("(?s)a.c", "a\nc"));
        assert_eq!(find_all("x", CASE_INSENSITIVE, "aXbx"), vec![(1, 2), (3, 4)]);
        assert!(matches("[a-c&&[^b]]+", "acca"));
        assert!(!matches("[a-c&&[^b]]+", "abc"));
        assert!(matches("\\d{3}-\\d{2,}", "123-4567"));
        assert!(matches("\\p{Lu}\\p{L}*", "Émile"));
        assert!(matches("\\Qa.b\\E", "a.b"));
        assert!(!matches("\\Qa.b\\E", "axb"));
    }

    #[test]
    fn groups_and_backrefs() {
        assert_eq!(groups("(\\d+)-(\\d+)", "tel 123-456"), vec![Some("123-456".into()), Some("123".into()), Some("456".into())]);
        assert_eq!(groups("(a)|(b)", "b"), vec![Some("b".into()), None, Some("b".into())]);
        assert_eq!(groups("(?:(a)|b)+", "ab"), vec![Some("ab".into()), Some("a".into())]);
        assert!(matches("(\\w)\\1", "aa"));
        assert!(!matches("(\\w)\\1", "ab"));
        assert!(matches("(?<x>a)\\k<x>", "aa"));
        assert_eq!(groups("(a*)*", "b"), vec![Some("".into()), Some("".into())]);
    }

    #[test]
    fn lookaround_and_atomic() {
        assert_eq!(find_all("a(?=b)", 0, "ab ac"), vec![(0, 1)]);
        assert_eq!(find_all("a(?!b)", 0, "ab ac"), vec![(3, 4)]);
        assert_eq!(find_all("(?<=x)a", 0, "xa ya"), vec![(1, 2)]);
        assert_eq!(find_all("(?<!x)a", 0, "xa ya"), vec![(4, 5)]);
        assert!(!matches("(?>a+)a", "aaa"));
        assert!(!matches("a++a", "aaa"));
        assert!(matches("a+?a", "aaa"));
    }

    #[test]
    fn anchors() {
        assert_eq!(find_all("^a", MULTILINE, "a\na\r\na"), vec![(0, 1), (2, 3), (5, 6)]);
        assert_eq!(find_all("a$", 0, "a\na\n"), vec![(2, 3)]);
        assert_eq!(find_all("a$", MULTILINE, "a\na\n"), vec![(0, 1), (2, 3)]);
        assert_eq!(find_all("\\bfoo\\b", 0, "foo foobar xfoo foo"), vec![(0, 3), (16, 19)]);
    }

    #[test]
    fn utf16_positions() {
        assert_eq!(find_all("b", 0, "😀b"), vec![(2, 3)]);
        assert_eq!(find_all(".", 0, "😀"), vec![(0, 2)]);
    }

    #[test]
    fn syntax_errors() {
        assert_eq!(perr("(ab").1, "Unclosed group");
        assert_eq!(perr("ab)").1, "Unmatched closing ')'");
        assert_eq!(perr("*a").1, "Dangling meta character '*'");
        assert_eq!(perr("[ab").1, "Unclosed character class");
        assert_eq!(perr("a{x}").1, "Illegal repetition");
        assert_eq!(perr("[z-a]").1, "Illegal character range");
        assert_eq!(perr("\\q").1, "Illegal/unsupported escape sequence");
        assert_eq!(perr("(?<=a+)b").1, "Look-behind group does not have an obvious maximum length");
    }

    #[test]
    fn replace_and_split() {
        let re = Arc::new(Regex::compile("(\\w)(\\d)", 0).unwrap());
        let mut m = MatcherState::new(re, "a1 b2", 0);
        assert_eq!(m.replace("$2$1", true).unwrap(), "1a 2b");
        assert_eq!(quote_replacement("a$b\\"), "a\\$b\\\\");
        assert_eq!(quote("a\\Eb"), "\\Qa\\E\\\\E\\Qb\\E");
    }

    #[test]
    fn limit_counts_reads() {
        let re = Arc::new(Regex::compile("(a+)+c", 0).unwrap());
        let mut m = MatcherState::new(re, "aaaaaaaaaaaaaaaaaaaab", 6);
        assert!(m.matches().is_err());
        let re = Arc::new(Regex::compile("a+b", 0).unwrap());
        let mut m = MatcherState::new(re, "aaab", 6);
        assert!(m.matches().unwrap());
    }
}
