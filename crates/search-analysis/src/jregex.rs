//! Java regular expressions (`java.util.regex.Pattern` and `Matcher`).
//!
//! Elasticsearch's `pattern` tokenizer and analyzer, `pattern_replace` (char and token filter),
//! `pattern_capture` and `keyword_marker.keywords_pattern` take Java patterns plus `flags`
//! (`CASE_INSENSITIVE|COMMENTS` …). Java and the `regex` crate disagree on what the common escapes
//! mean (`\w`, `\d`, `\s`, `\b` and the POSIX classes are ASCII in Java unless
//! `UNICODE_CHARACTER_CLASS` is set; `.` also stops at `\r`, `\u0085`, `\u2028`, `\u2029`;
//! `CASE_INSENSITIVE` folds ASCII only without `UNICODE_CASE`; `$` also matches before a final line
//! terminator), and Java has features `regex` lacks (lookaround, backreferences, atomic groups,
//! possessive quantifiers). So patterns are parsed here with Java's grammar into an AST whose
//! character classes are already resolved to code-point sets with Java's semantics, and then run
//! by one of two engines:
//!
//! - **fast**: when the AST only uses regular constructs, it is rendered back to `regex` syntax
//!   (explicit ranges, so nothing is reinterpreted) and run by the `regex` crate. Its
//!   leftmost-first semantics pick the same match as Java's backtracking. Two Java behaviours that
//!   depend on the input are checked per call: `$`/`\Z` before a trailing line terminator, and
//!   ASCII `\b` next to a non-spacing mark (Java counts a mark after a letter as part of the
//!   word); inputs where they matter go to the backtracker.
//! - **backtracking**: a continuation-passing matcher over the AST with Java's semantics for
//!   everything else. Single-character repetitions loop instead of recursing, so typical patterns
//!   run in constant stack; deeply nested repetitions of groups stop at a depth limit (where Java
//!   would overflow its stack) and count as no match.
//!
//! [`Matcher`] reproduces `Matcher.find()`'s iteration rule (after an empty match the next search
//! starts one character later) and [`Replacement`] `appendReplacement`'s `$n`/`${name}`/`\`
//! syntax and errors. Offsets are byte offsets into the `&str`; callers convert to UTF-16.

use std::sync::{Arc, OnceLock};

use crate::dfa::{self, Ranges};
use crate::error::{AnalysisError, Result};

pub(crate) const UNIX_LINES: u32 = 0x01;
pub(crate) const CASE_INSENSITIVE: u32 = 0x02;
pub(crate) const COMMENTS: u32 = 0x04;
pub(crate) const MULTILINE: u32 = 0x08;
pub(crate) const LITERAL: u32 = 0x10;
pub(crate) const DOTALL: u32 = 0x20;
pub(crate) const UNICODE_CASE: u32 = 0x40;
pub(crate) const CANON_EQ: u32 = 0x80;
pub(crate) const UNICODE_CHARACTER_CLASS: u32 = 0x100;

/// Elasticsearch's `Regex.flagsFromString` (`|`-separated, case-insensitive names).
pub(crate) fn flags_from_string(flags: &str) -> Result<u32> {
    let mut f = 0;
    for s in flags.split('|') {
        if s.is_empty() {
            continue;
        }
        let s = s.to_uppercase();
        f |= match s.as_str() {
            "CASE_INSENSITIVE" => CASE_INSENSITIVE,
            "MULTILINE" => MULTILINE,
            "DOTALL" => DOTALL,
            "UNICODE_CASE" => UNICODE_CASE,
            "CANON_EQ" => CANON_EQ,
            "UNIX_LINES" => UNIX_LINES,
            "LITERAL" => LITERAL,
            "COMMENTS" => COMMENTS,
            "UNICODE_CHAR_CLASS" | "UNICODE_CHARACTER_CLASS" => UNICODE_CHARACTER_CLASS,
            _ => return Err(AnalysisError::illegal(format!("Unknown regex flag [{s}]"))),
        };
    }
    Ok(f)
}

// ---------------------------------------------------------------------------------------------
// AST.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Greed {
    Greedy,
    Lazy,
    Possessive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    /// `\A`, or `^` without MULTILINE.
    Start,
    /// `^` with MULTILINE (`unix`: UNIX_LINES).
    StartLine { unix: bool },
    /// `\z`.
    End,
    /// `$` without MULTILINE, `\Z`: the end, or before a final line terminator.
    EndOrFinalTerminator { unix: bool },
    /// `$` with MULTILINE.
    EndLine { unix: bool },
    /// `\b` / `\B` (`unicode`: UNICODE_CHARACTER_CLASS).
    Boundary { unicode: bool, negate: bool },
    /// `\G`: where the search started.
    SearchStart,
    /// `\b{g}`: an extended grapheme cluster boundary.
    Grapheme,
}

#[derive(Clone, Debug)]
enum Node {
    Empty,
    /// One code point from the set.
    Set(Arc<[(u32, u32)]>),
    /// An exact string.
    Str(Box<str>),
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    /// A group; `Some(i)` captures into group `i`.
    Group(Box<Node>, Option<usize>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greed: Greed,
    },
    Look(Look),
    Around {
        node: Box<Node>,
        behind: bool,
        negate: bool,
    },
    Atomic(Box<Node>),
    Backref {
        group: usize,
        ci: bool,
        unicode_case: bool,
    },
    /// `\X`: one extended grapheme cluster.
    Grapheme,
}

impl Node {
    /// Whether the `regex` crate can run it with the same results.
    fn is_regular(&self, f: &mut Features) -> bool {
        match self {
            Node::Empty | Node::Set(_) | Node::Str(_) => true,
            Node::Seq(v) | Node::Alt(v) => v.iter().all(|n| n.is_regular(f)),
            Node::Group(n, _) => n.is_regular(f),
            Node::Repeat { node, greed, .. } => {
                *greed != Greed::Possessive && !node.java_loop_matters() && node.is_regular(f)
            }
            Node::Look(l) => match l {
                Look::Start | Look::End => true,
                Look::EndOrFinalTerminator { unix } => {
                    f.dollar = true;
                    f.dollar_unix &= *unix;
                    true
                }
                Look::Boundary { unicode, .. } => {
                    if !unicode {
                        f.ascii_boundary = true;
                    }
                    true
                }
                _ => false,
            },
            Node::Around { .. } | Node::Atomic(_) | Node::Backref { .. } | Node::Grapheme => false,
        }
    }

    /// `TreeInfo.deterministic` after Java's `study()`: no alternation, `?` or variable-count
    /// repetition inside. A quantified group whose body is not deterministic compiles to
    /// Java's `Loop`, otherwise to `GroupCurly`.
    fn java_deterministic(&self) -> bool {
        match self {
            Node::Empty
            | Node::Set(_)
            | Node::Str(_)
            | Node::Look(_)
            | Node::Backref { .. }
            | Node::Around { .. } => true,
            Node::Seq(v) => v.iter().all(Node::java_deterministic),
            Node::Alt(_) | Node::Grapheme => false,
            Node::Group(n, _) | Node::Atomic(n) => n.java_deterministic(),
            Node::Repeat { node, min, max, .. } => Some(*min) == *max && node.java_deterministic(),
        }
    }

    /// Whether repeating this node follows Java's `Loop` rules where they differ from plain
    /// backtracking: a body with choices that can also match the empty string. (Java ends the
    /// loop after an empty iteration, accepting it even below the minimum count, instead of
    /// backtracking into the body for a longer alternative.)
    fn java_loop_matters(&self) -> bool {
        !self.java_deterministic() && self.len16().0 == 0
    }

    /// The minimum and maximum length in UTF-16 units (None: unbounded).
    fn len16(&self) -> (usize, Option<usize>) {
        match self {
            Node::Empty | Node::Look(_) | Node::Around { .. } => (0, Some(0)),
            Node::Set(r) => {
                let bmp = r.first().is_some_and(|&(lo, _)| lo < 0x10000);
                let supp = r.last().is_some_and(|&(_, hi)| hi >= 0x10000);
                (if bmp { 1 } else { 2 }, Some(if supp { 2 } else { 1 }))
            }
            Node::Str(s) => {
                let n = crate::unicode::utf16_len(s);
                (n, Some(n))
            }
            Node::Seq(v) => v.iter().fold((0, Some(0)), |(a, b), n| {
                let (x, y) = n.len16();
                (a + x, b.zip(y).map(|(b, y)| b + y))
            }),
            Node::Alt(v) => {
                let mut it = v.iter().map(Node::len16);
                let first = it.next().unwrap_or((0, Some(0)));
                it.fold(first, |(a, b), (x, y)| {
                    (a.min(x), b.zip(y).map(|(b, y)| b.max(y)))
                })
            }
            Node::Group(n, _) | Node::Atomic(n) => n.len16(),
            Node::Repeat { node, min, max, .. } => {
                let (x, y) = node.len16();
                (
                    x.saturating_mul(*min as usize),
                    match (y, max) {
                        (Some(0), _) => Some(0),
                        (Some(y), Some(m)) => Some(y.saturating_mul(*m as usize)),
                        _ => None,
                    },
                )
            }
            Node::Backref { .. } => (0, None),
            Node::Grapheme => (1, None),
        }
    }
}

/// Input-dependent behaviours of a pattern the fast engine needs checked per call.
#[derive(Clone, Copy, Debug)]
struct Features {
    dollar: bool,
    /// All `$`s are UNIX_LINES ones (only `\n` counts as a terminator).
    dollar_unix: bool,
    ascii_boundary: bool,
}

// ---------------------------------------------------------------------------------------------
// Compiled patterns.

/// A compiled Java pattern.
pub(crate) struct JavaRegex {
    source: String,
    root: Node,
    fast: Option<Fast>,
    /// Capturing groups, not counting group 0 (`Matcher.groupCount()`).
    pub groups: usize,
    names: Vec<(String, usize)>,
}

struct Fast {
    /// The pattern with `$`/`\Z` as `\z` (valid when the input has no final line terminator).
    re: regex::Regex,
    /// Anchored at both ends, for [`JavaRegex::full_match`] (built on first use).
    full: OnceLock<Option<regex::Regex>>,
    rendered: String,
    features: Features,
}

impl std::fmt::Debug for JavaRegex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JavaRegex({:?})", self.source)
    }
}

/// `Pattern.compile(pattern, flags)`.
pub(crate) fn compile(pattern: &str, flags: u32) -> Result<JavaRegex> {
    let mut p = Parser::new(pattern, flags);
    let root = if flags & LITERAL != 0 {
        literal_node(pattern, Flags::from(flags))
    } else {
        p.parse().map_err(|e| e.into_error(pattern))?
    };
    let mut features = Features {
        dollar: false,
        dollar_unix: true,
        ascii_boundary: false,
    };
    let fast = if root.is_regular(&mut features) {
        let mut rendered = String::with_capacity(pattern.len() * 2);
        render(&root, &mut rendered);
        regex::RegexBuilder::new(&rendered)
            .size_limit(64 << 20)
            .dfa_size_limit(16 << 20)
            .build()
            .ok()
            .map(|re| Fast {
                re,
                full: OnceLock::new(),
                rendered,
                features,
            })
    } else {
        None
    };
    Ok(JavaRegex {
        source: pattern.to_string(),
        root,
        fast,
        groups: p.groups,
        names: p.names,
    })
}

fn literal_node(s: &str, f: Flags) -> Node {
    let mut v: Vec<Node> = s.chars().map(|c| f.literal(c)).collect();
    merge_literals(&mut v);
    match v.len() {
        0 => Node::Empty,
        1 => v.pop().unwrap_or(Node::Empty),
        _ => Node::Seq(v),
    }
}

/// Line terminators for `.`, `^`, `$` (`\u0085`, `\u2028`, `\u2029` plus `\n`, `\r`).
fn is_terminator(c: char, unix: bool) -> bool {
    if unix {
        c == '\n'
    } else {
        matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
    }
}

impl JavaRegex {
    /// The group number of a named group.
    pub fn group_index(&self, name: &str) -> Option<usize> {
        self.names.iter().find(|(n, _)| n == name).map(|&(_, i)| i)
    }

    /// Scratch space for matching (reuse it across calls to avoid allocating).
    pub fn captures(&self) -> Captures {
        Captures {
            slots: vec![None; 2 * (self.groups + 1)],
            locs: self.fast.as_ref().map(|f| f.re.capture_locations()),
        }
    }

    /// Whether the fast engine gives Java's answer on `text`.
    fn fast_for(&self, text: &str) -> Option<&Fast> {
        let f = self.fast.as_ref()?;
        if f.features.dollar
            && let Some(c) = text.chars().next_back()
            && is_terminator(c, f.features.dollar_unix)
        {
            return None;
        }
        if f.features.ascii_boundary
            && !text.is_ascii()
            && text
                .chars()
                .any(|c| crate::unicode::get_type(c) == crate::unicode::gc::NON_SPACING_MARK)
        {
            return None;
        }
        Some(f)
    }

    /// The first match starting at or after byte `start` (`Matcher.find(int)` without the
    /// reset), filling `caps`. Returns whether there is one.
    pub fn find_at(&self, text: &str, start: usize, caps: &mut Captures) -> bool {
        if start > text.len() {
            return false;
        }
        if let Some(f) = self.fast_for(text) {
            let locs = caps.locs.get_or_insert_with(|| f.re.capture_locations());
            if f.re.captures_read_at(locs, text, start).is_none() {
                return false;
            }
            for i in 0..=self.groups {
                let g = locs.get(i);
                caps.slots[2 * i] = g.map(|g| g.0);
                caps.slots[2 * i + 1] = g.map(|g| g.1);
            }
            return true;
        }
        let mut bt = Backtracker::new(text, self.groups, start);
        let mut s = start;
        loop {
            bt.reset();
            let found = bt.run(&self.root, s, &mut |_, p| Some(p));
            if let Some(end) = found {
                bt.slots[0] = Some(s);
                bt.slots[1] = Some(end);
                caps.slots.copy_from_slice(&bt.slots);
                return true;
            }
            if s >= text.len() {
                return false;
            }
            s += text[s..].chars().next().map_or(1, char::len_utf8);
        }
    }

    /// The bounds of the first match at or after `start`, without groups (faster).
    pub fn find_bounds_at(
        &self,
        text: &str,
        start: usize,
        caps: &mut Captures,
    ) -> Option<(usize, usize)> {
        if start > text.len() {
            return None;
        }
        if let Some(f) = self.fast_for(text) {
            return f.re.find_at(text, start).map(|m| (m.start(), m.end()));
        }
        if self.find_at(text, start, caps) {
            caps.get(0)
        } else {
            None
        }
    }

    /// `Matcher.matches()`: whether the whole text matches.
    pub fn full_match(&self, text: &str) -> bool {
        if let Some(f) = self.fast_for(text) {
            let full = f.full.get_or_init(|| {
                regex::RegexBuilder::new(&format!("\\A(?:{})\\z", f.rendered))
                    .size_limit(64 << 20)
                    .build()
                    .ok()
            });
            if let Some(full) = full {
                return full.is_match(text);
            }
        }
        let mut bt = Backtracker::new(text, self.groups, 0);
        let len = text.len();
        bt.run(&self.root, 0, &mut |_, p| (p == len).then_some(p))
            .is_some()
    }
}

/// Match groups as byte ranges (Java's `start(i)`/`end(i)`), plus the fast engine's scratch.
pub(crate) struct Captures {
    slots: Vec<Option<usize>>,
    locs: Option<regex::CaptureLocations>,
}

impl Captures {
    #[inline]
    pub fn get(&self, i: usize) -> Option<(usize, usize)> {
        Some((
            self.slots.get(2 * i).copied()??,
            self.slots.get(2 * i + 1).copied()??,
        ))
    }
}

/// `Matcher.find()` iteration: the next search starts at the end of the previous match, or one
/// character after it when that match was empty.
pub(crate) struct Finder {
    next: usize,
    done: bool,
}

impl Finder {
    pub fn new() -> Finder {
        Finder {
            next: 0,
            done: false,
        }
    }

    pub fn reset(&mut self) {
        self.next = 0;
        self.done = false;
    }

    /// The next match, with groups in `caps` when `groups` is set (otherwise only group 0).
    pub fn next(
        &mut self,
        re: &JavaRegex,
        text: &str,
        caps: &mut Captures,
        groups: bool,
    ) -> Option<(usize, usize)> {
        if self.done || self.next > text.len() {
            self.done = true;
            return None;
        }
        let m = if groups {
            if re.find_at(text, self.next, caps) {
                caps.get(0)
            } else {
                None
            }
        } else {
            re.find_bounds_at(text, self.next, caps)
        };
        match m {
            None => {
                self.done = true;
                None
            }
            Some((s, e)) => {
                self.next = if s == e {
                    e + text[e..].chars().next().map_or(1, char::len_utf8)
                } else {
                    e
                };
                if !groups {
                    caps.slots[0] = Some(s);
                    caps.slots[1] = Some(e);
                }
                Some((s, e))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Replacement strings.

#[derive(Clone, Debug)]
enum RepPart {
    Lit(String),
    Group(usize),
}

/// A replacement string in `Matcher.appendReplacement` syntax. Java parses it while appending,
/// so a bad replacement only fails once something matched; the error is kept until then.
#[derive(Clone, Debug)]
pub(crate) struct Replacement {
    parts: std::result::Result<Vec<RepPart>, AnalysisError>,
}

impl Replacement {
    pub fn parse(rep: &str, re: &JavaRegex) -> Replacement {
        Replacement {
            parts: parse_replacement(rep, re),
        }
    }

    /// Appends the replacement for the match in `caps` to `out`.
    pub fn append(&self, text: &str, caps: &Captures, out: &mut String) -> Result<()> {
        let parts = self.parts.as_ref().map_err(Clone::clone)?;
        for p in parts {
            match p {
                RepPart::Lit(s) => out.push_str(s),
                RepPart::Group(g) => {
                    if let Some((s, e)) = caps.get(*g) {
                        out.push_str(&text[s..e]);
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether the replacement uses groups (so matching must record them).
    pub fn uses_groups(&self) -> bool {
        match &self.parts {
            Ok(p) => p.iter().any(|p| matches!(p, RepPart::Group(g) if *g > 0)),
            Err(_) => false,
        }
    }

    /// `Matcher.replaceAll` / `replaceFirst` into `out`; returns whether anything matched.
    pub fn replace(
        &self,
        re: &JavaRegex,
        text: &str,
        all: bool,
        caps: &mut Captures,
        out: &mut String,
    ) -> Result<bool> {
        let mut finder = Finder::new();
        let groups = self.uses_groups();
        let mut last = 0;
        let mut any = false;
        while let Some((s, e)) = finder.next(re, text, caps, groups) {
            any = true;
            out.push_str(&text[last..s]);
            self.append(text, caps, out)?;
            last = e;
            if !all {
                break;
            }
        }
        out.push_str(&text[last..]);
        Ok(any)
    }
}

fn parse_replacement(
    rep: &str,
    re: &JavaRegex,
) -> std::result::Result<Vec<RepPart>, AnalysisError> {
    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut it = rep.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.next() {
                Some(n) => lit.push(n),
                None => return Err(AnalysisError::illegal("character to be escaped is missing")),
            },
            '$' => {
                let Some(&n) = it.peek() else {
                    return Err(AnalysisError::illegal(
                        "Illegal group reference: group index is missing",
                    ));
                };
                let group = if n == '{' {
                    it.next();
                    let mut name = String::new();
                    loop {
                        match it.next() {
                            Some('}') => break,
                            Some(ch) if ch.is_ascii_alphanumeric() => name.push(ch),
                            _ => {
                                return Err(AnalysisError::illegal(
                                    "named capturing group is missing trailing '}'",
                                ));
                            }
                        }
                    }
                    if name.is_empty() {
                        return Err(AnalysisError::illegal(
                            "named capturing group has 0 length name",
                        ));
                    }
                    if name.as_bytes()[0].is_ascii_digit() {
                        return Err(AnalysisError::illegal(
                            "capturing group name {".to_string()
                                + &name
                                + "} starts with digit character",
                        ));
                    }
                    match re.group_index(&name) {
                        Some(g) => g,
                        None => {
                            return Err(AnalysisError::illegal(format!(
                                "No group with name {{{name}}}"
                            )));
                        }
                    }
                } else {
                    let Some(d) = n.to_digit(10) else {
                        return Err(AnalysisError::illegal("Illegal group reference"));
                    };
                    it.next();
                    let mut g = d as usize;
                    while let Some(&n) = it.peek() {
                        let Some(d) = n.to_digit(10) else { break };
                        let ng = g * 10 + d as usize;
                        if ng > re.groups {
                            break;
                        }
                        g = ng;
                        it.next();
                    }
                    if g > re.groups {
                        return Err(AnalysisError::other(
                            "index_out_of_bounds_exception",
                            500,
                            format!("No group {g}"),
                        ));
                    }
                    g
                };
                if !lit.is_empty() {
                    parts.push(RepPart::Lit(std::mem::take(&mut lit)));
                }
                parts.push(RepPart::Group(group));
            }
            c => lit.push(c),
        }
    }
    if !lit.is_empty() {
        parts.push(RepPart::Lit(lit));
    }
    Ok(parts)
}

// ---------------------------------------------------------------------------------------------
// Rendering to `regex` syntax.

fn push_cp(out: &mut String, cp: u32) {
    use std::fmt::Write;
    let _ = write!(out, "\\x{{{cp:X}}}");
}

fn render_set(r: &[(u32, u32)], out: &mut String) {
    // Surrogates are not scalar values; `regex` rejects them in classes.
    let mut items: Vec<(u32, u32)> = Vec::with_capacity(r.len() + 1);
    for &(lo, hi) in r {
        if hi < 0xD800 || lo > 0xDFFF {
            items.push((lo, hi));
        } else {
            if lo < 0xD800 {
                items.push((lo, 0xD7FF));
            }
            if hi > 0xDFFF {
                items.push((0xE000, hi));
            }
        }
    }
    if items.is_empty() {
        // Matches nothing.
        out.push_str("[^\\x{0}-\\x{10FFFF}]");
        return;
    }
    out.push('[');
    for (lo, hi) in items {
        push_cp(out, lo);
        if hi != lo {
            out.push('-');
            push_cp(out, hi);
        }
    }
    out.push(']');
}

fn render(n: &Node, out: &mut String) {
    match n {
        Node::Empty => out.push_str("(?:)"),
        Node::Set(r) => render_set(r, out),
        Node::Str(s) => {
            for c in s.chars() {
                if c.is_ascii_alphanumeric() {
                    out.push(c);
                } else {
                    push_cp(out, c as u32);
                }
            }
        }
        Node::Seq(v) => {
            out.push_str("(?:");
            for x in v {
                render(x, out);
            }
            out.push(')');
        }
        Node::Alt(v) => {
            out.push_str("(?:");
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push('|');
                }
                render(x, out);
            }
            out.push(')');
        }
        Node::Group(x, cap) => {
            out.push_str(if cap.is_some() { "(" } else { "(?:" });
            render(x, out);
            out.push(')');
        }
        Node::Repeat {
            node,
            min,
            max,
            greed,
        } => {
            out.push_str("(?:");
            render(node, out);
            out.push(')');
            match max {
                Some(m) => out.push_str(&format!("{{{min},{m}}}")),
                None => out.push_str(&format!("{{{min},}}")),
            }
            if *greed == Greed::Lazy {
                out.push('?');
            }
        }
        Node::Look(l) => out.push_str(match l {
            Look::Start => "\\A",
            Look::End | Look::EndOrFinalTerminator { .. } => "\\z",
            Look::Boundary {
                unicode: true,
                negate: false,
            } => "\\b",
            Look::Boundary {
                unicode: true,
                negate: true,
            } => "\\B",
            Look::Boundary {
                unicode: false,
                negate: false,
            } => "(?-u:\\b)",
            Look::Boundary {
                unicode: false,
                negate: true,
            } => "(?-u:\\B)",
            _ => unreachable!("not a regular construct"),
        }),
        Node::Around { .. } | Node::Atomic(_) | Node::Backref { .. } | Node::Grapheme => {
            unreachable!("not a regular construct")
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The backtracking engine.

/// Recursion budget of one match attempt (Java overflows its stack instead).
const MAX_DEPTH: u32 = 3000;
/// Work budget of one search (pathological patterns).
const MAX_STEPS: u64 = 50_000_000;

type K<'k> = &'k mut dyn FnMut(&mut Backtracker<'_>, usize) -> Option<usize>;

struct Backtracker<'t> {
    text: &'t str,
    slots: Vec<Option<usize>>,
    search_start: usize,
    depth: u32,
    steps: u64,
}

#[inline]
fn char_at(text: &str, pos: usize) -> Option<char> {
    text[pos..].chars().next()
}

#[inline]
fn char_before(text: &str, pos: usize) -> Option<char> {
    text[..pos].chars().next_back()
}

fn is_ascii_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_unicode_word(c: char) -> bool {
    use crate::unicode::gc;
    crate::unicode::is_alphabetic(c)
        || matches!(
            crate::unicode::get_type(c),
            gc::NON_SPACING_MARK
                | gc::ENCLOSING_MARK
                | gc::COMBINING_SPACING_MARK
                | gc::DECIMAL_DIGIT_NUMBER
                | gc::CONNECTOR_PUNCTUATION
        )
        || c == '\u{200C}'
        || c == '\u{200D}'
}

impl<'t> Backtracker<'t> {
    fn new(text: &'t str, groups: usize, search_start: usize) -> Self {
        Backtracker {
            text,
            slots: vec![None; 2 * (groups + 1)],
            search_start,
            depth: 0,
            steps: 0,
        }
    }

    fn reset(&mut self) {
        self.slots.iter_mut().for_each(|s| *s = None);
        self.depth = 0;
    }

    /// Matches `n` at `pos` and then the continuation; returns the continuation's result.
    fn run(&mut self, n: &Node, pos: usize, k: K<'_>) -> Option<usize> {
        self.steps += 1;
        if self.steps > MAX_STEPS || self.depth > MAX_DEPTH {
            return None;
        }
        self.depth += 1;
        let r = self.run_inner(n, pos, k);
        self.depth -= 1;
        r
    }

    fn run_inner(&mut self, n: &Node, pos: usize, k: K<'_>) -> Option<usize> {
        match n {
            Node::Empty => k(self, pos),
            Node::Set(r) => {
                let c = char_at(self.text, pos)?;
                if dfa::contains(r, c as u32) {
                    k(self, pos + c.len_utf8())
                } else {
                    None
                }
            }
            Node::Str(s) => {
                if self.text[pos..].starts_with(&**s) {
                    k(self, pos + s.len())
                } else {
                    None
                }
            }
            Node::Seq(v) => self.seq(v, pos, k),
            Node::Alt(v) => {
                for a in v {
                    if let Some(r) = self.run(a, pos, k) {
                        return Some(r);
                    }
                }
                None
            }
            Node::Group(x, None) => self.run(x, pos, k),
            Node::Group(x, Some(g)) => {
                let g = *g;
                let (old_s, old_e) = (self.slots[2 * g], self.slots[2 * g + 1]);
                let r = self.run(x, pos, &mut |bt: &mut Backtracker<'_>, p| {
                    let (in_s, in_e) = (bt.slots[2 * g], bt.slots[2 * g + 1]);
                    bt.slots[2 * g] = Some(pos);
                    bt.slots[2 * g + 1] = Some(p);
                    let r = k(bt, p);
                    if r.is_none() {
                        bt.slots[2 * g] = in_s;
                        bt.slots[2 * g + 1] = in_e;
                    }
                    r
                });
                if r.is_none() {
                    self.slots[2 * g] = old_s;
                    self.slots[2 * g + 1] = old_e;
                }
                r
            }
            Node::Repeat {
                node,
                min,
                max,
                greed,
            } => {
                if matches!(**node, Node::Set(_) | Node::Str(_)) {
                    return self.repeat_simple(node, *min, *max, *greed, pos, k);
                }
                if *greed != Greed::Possessive && node.java_loop_matters() {
                    return self.java_loop(node, *min, *max, *greed == Greed::Lazy, pos, k);
                }
                match greed {
                    Greed::Greedy => self.greedy(node, *min, *max, 0, pos, k),
                    Greed::Lazy => self.lazy(node, *min, *max, 0, pos, k),
                    Greed::Possessive => {
                        // Java's possessive `Curly`: each iteration takes the body's first
                        // match, an empty one ends the loop, and nothing is given back.
                        let saved = self.slots.clone();
                        let mut i = pos;
                        let mut count = 0u32;
                        while count < *min {
                            match self.run(node, i, &mut |_, p| Some(p)) {
                                Some(p) => i = p,
                                None => {
                                    self.slots = saved;
                                    return None;
                                }
                            }
                            count += 1;
                        }
                        while max.is_none_or(|m| count < m) {
                            match self.run(node, i, &mut |_, p| Some(p)) {
                                Some(p) if p != i => i = p,
                                _ => break,
                            }
                            count += 1;
                        }
                        let r = k(self, i);
                        if r.is_none() {
                            self.slots = saved;
                        }
                        r
                    }
                }
            }
            Node::Look(l) => {
                if self.look(*l, pos) {
                    k(self, pos)
                } else {
                    None
                }
            }
            Node::Grapheme => {
                // Java's `XGrapheme`: the cluster is segmented afresh from `pos`.
                use unicode_segmentation::UnicodeSegmentation;
                let g = self.text[pos..].graphemes(true).next()?;
                k(self, pos + g.len())
            }
            Node::Atomic(x) => {
                let saved = self.slots.clone();
                match self.run(x, pos, &mut |_, p| Some(p)) {
                    Some(e) => {
                        let r = k(self, e);
                        if r.is_none() {
                            self.slots = saved;
                        }
                        r
                    }
                    None => None,
                }
            }
            Node::Around {
                node,
                behind: false,
                negate,
            } => {
                let saved = self.slots.clone();
                let matched = self.run(node, pos, &mut |_, p| Some(p)).is_some();
                if matched == *negate {
                    self.slots = saved;
                    return None;
                }
                if *negate {
                    self.slots = saved.clone();
                }
                let r = k(self, pos);
                if r.is_none() {
                    self.slots = saved;
                }
                r
            }
            Node::Around {
                node,
                behind: true,
                negate,
            } => {
                let saved = self.slots.clone();
                let (min, max) = node.len16();
                let mut matched = false;
                // Candidate starts from the closest (shortest) to the farthest, as Java tries them.
                let mut units = 0usize;
                let mut s = pos;
                loop {
                    if units >= min && max.is_none_or(|m| units <= m) {
                        let target = pos;
                        if self
                            .run(node, s, &mut |_, p| (p == target).then_some(p))
                            .is_some()
                        {
                            matched = true;
                            break;
                        }
                    }
                    if max.is_some_and(|m| units >= m) {
                        break;
                    }
                    match char_before(self.text, s) {
                        Some(c) => {
                            s -= c.len_utf8();
                            units += c.len_utf16();
                        }
                        None => break,
                    }
                }
                if matched == *negate {
                    self.slots = saved;
                    return None;
                }
                if *negate {
                    self.slots = saved.clone();
                }
                let r = k(self, pos);
                if r.is_none() {
                    self.slots = saved;
                }
                r
            }
            Node::Backref {
                group,
                ci,
                unicode_case,
            } => {
                let (Some(s), Some(e)) = (
                    self.slots.get(2 * group).copied().flatten(),
                    self.slots.get(2 * group + 1).copied().flatten(),
                ) else {
                    return None;
                };
                let want = &self.text[s..e];
                if !ci {
                    return if self.text[pos..].starts_with(want) {
                        k(self, pos + want.len())
                    } else {
                        None
                    };
                }
                let mut p = pos;
                for w in want.chars() {
                    let c = char_at(self.text, p)?;
                    let same = c == w
                        || if *unicode_case {
                            crate::unicode::to_upper(c) == crate::unicode::to_upper(w)
                                || crate::unicode::to_lower(c) == crate::unicode::to_lower(w)
                        } else {
                            c.is_ascii() && w.is_ascii() && c.eq_ignore_ascii_case(&w)
                        };
                    if !same {
                        return None;
                    }
                    p += c.len_utf8();
                }
                k(self, p)
            }
        }
    }

    fn seq(&mut self, v: &[Node], pos: usize, k: K<'_>) -> Option<usize> {
        match v.split_first() {
            None => k(self, pos),
            Some((first, [])) => self.run(first, pos, k),
            Some((first, rest)) => self.run(first, pos, &mut |bt: &mut Backtracker<'_>, p| {
                bt.seq(rest, p, k)
            }),
        }
    }

    /// Repetition of a node that consumes a fixed string or one character: positions are
    /// collected iteratively, then the continuation is tried at each (Java's `Curly`).
    fn repeat_simple(
        &mut self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        greed: Greed,
        pos: usize,
        k: K<'_>,
    ) -> Option<usize> {
        let max = max.unwrap_or(u32::MAX);
        let step = |text: &str, p: usize| -> Option<usize> {
            match node {
                Node::Set(r) => {
                    let c = char_at(text, p)?;
                    dfa::contains(r, c as u32).then(|| p + c.len_utf8())
                }
                Node::Str(s) => text[p..].starts_with(&**s).then(|| p + s.len()),
                _ => None,
            }
        };
        let mut p = pos;
        let mut count = 0u32;
        while count < min {
            p = step(self.text, p)?;
            count += 1;
        }
        if greed == Greed::Lazy {
            loop {
                if let Some(r) = k(self, p) {
                    return Some(r);
                }
                if count >= max {
                    return None;
                }
                p = step(self.text, p)?;
                count += 1;
                self.steps += 1;
                if self.steps > MAX_STEPS {
                    return None;
                }
            }
        }
        // Greedy and possessive: consume as many as possible.
        let mut ends: smallvec::SmallVec<[usize; 32]> = smallvec::SmallVec::new();
        ends.push(p);
        while count < max {
            match step(self.text, p) {
                Some(np) if np != p => {
                    p = np;
                    count += 1;
                    if greed == Greed::Greedy {
                        ends.push(p);
                    }
                }
                _ => break,
            }
        }
        if greed == Greed::Possessive {
            return k(self, p);
        }
        while let Some(e) = ends.pop() {
            if let Some(r) = k(self, e) {
                return Some(r);
            }
            self.steps += 1;
            if self.steps > MAX_STEPS {
                return None;
            }
        }
        None
    }

    /// Java's `Prolog`/`Loop` (`LazyLoop` when `lazy`): the first iteration is tried without the
    /// empty-iteration check, see [`Backtracker::loop_end`].
    fn java_loop(
        &mut self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        lazy: bool,
        pos: usize,
        k: K<'_>,
    ) -> Option<usize> {
        let body = |bt: &mut Backtracker<'_>, k: K<'_>| {
            bt.run(node, pos, &mut |bt: &mut Backtracker<'_>, p| {
                bt.loop_end(node, min, max, lazy, pos, p, 1, k)
            })
        };
        if min > 0 {
            return body(self, k);
        }
        if lazy {
            if let Some(r) = k(self, pos) {
                return Some(r);
            }
            return if max != Some(0) { body(self, k) } else { None };
        }
        if max != Some(0)
            && let Some(r) = body(self, k)
        {
            return Some(r);
        }
        k(self, pos)
    }

    /// Java's `Loop.match` / `LazyLoop.match`, run where an iteration that started at `begin`
    /// ended (`pos`) with `count` iterations done.
    #[allow(clippy::too_many_arguments)]
    fn loop_end(
        &mut self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        lazy: bool,
        begin: usize,
        pos: usize,
        count: u32,
        k: K<'_>,
    ) -> Option<usize> {
        if pos <= begin {
            // An empty iteration ends the loop, whatever the count.
            return k(self, pos);
        }
        let again = |bt: &mut Backtracker<'_>, k: K<'_>| {
            bt.run(node, pos, &mut |bt: &mut Backtracker<'_>, p| {
                bt.loop_end(node, min, max, lazy, pos, p, count + 1, k)
            })
        };
        if count < min {
            return again(self, k);
        }
        let more = max.is_none_or(|m| count < m);
        if lazy {
            if let Some(r) = k(self, pos) {
                return Some(r);
            }
            return if more { again(self, k) } else { None };
        }
        if more && let Some(r) = again(self, k) {
            return Some(r);
        }
        k(self, pos)
    }

    fn greedy(
        &mut self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        count: u32,
        pos: usize,
        k: K<'_>,
    ) -> Option<usize> {
        if max.is_none_or(|m| count < m) {
            let r = self.run(node, pos, &mut |bt: &mut Backtracker<'_>, p| {
                // An empty iteration past the minimum would loop forever (Java's `Loop` stops too).
                if p == pos && count >= min {
                    return None;
                }
                bt.greedy(node, min, max, count + 1, p, k)
            });
            if r.is_some() {
                return r;
            }
        }
        if count >= min { k(self, pos) } else { None }
    }

    fn lazy(
        &mut self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        count: u32,
        pos: usize,
        k: K<'_>,
    ) -> Option<usize> {
        if count >= min
            && let Some(r) = k(self, pos)
        {
            return Some(r);
        }
        if max.is_some_and(|m| count >= m) {
            return None;
        }
        self.run(node, pos, &mut |bt: &mut Backtracker<'_>, p| {
            if p == pos && count >= min {
                return None;
            }
            bt.lazy(node, min, max, count + 1, p, k)
        })
    }

    fn look(&self, l: Look, pos: usize) -> bool {
        let text = self.text;
        match l {
            Look::Start => pos == 0,
            Look::End => pos == text.len(),
            Look::SearchStart => pos == self.search_start,
            Look::Grapheme => {
                // Java's `GraphemeBound` only compares `pos` with the first cluster end after
                // the previous match (approximated by the cluster containing the search start):
                // anything at or past it counts as a boundary.
                use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};
                if pos == 0 || pos >= text.len() {
                    return true;
                }
                let s = self.search_start.min(text.len());
                let mut c = GraphemeCursor::new(s, text.len(), true);
                let last = if c.is_boundary(text, 0).unwrap_or(true) {
                    s
                } else {
                    c.prev_boundary(text, 0).ok().flatten().unwrap_or(0)
                };
                let next = last + text[last..].graphemes(true).next().map_or(0, str::len);
                next <= pos
            }
            Look::StartLine { unix } => {
                // Java's `Caret`: never at the very end, not between `\r` and `\n`.
                if pos == text.len() {
                    return false;
                }
                if pos == 0 {
                    return true;
                }
                let prev = char_before(text, pos).unwrap_or(' ');
                if !is_terminator(prev, unix) {
                    return false;
                }
                !(!unix && prev == '\r' && char_at(text, pos) == Some('\n'))
            }
            Look::EndLine { unix } => {
                if pos == text.len() {
                    return true;
                }
                let c = char_at(text, pos).unwrap_or(' ');
                if !is_terminator(c, unix) {
                    return false;
                }
                !(c == '\n' && !unix && char_before(text, pos) == Some('\r'))
            }
            Look::EndOrFinalTerminator { unix } => {
                let rest = &text[pos..];
                if rest.is_empty() {
                    return true;
                }
                if unix {
                    return rest == "\n";
                }
                if rest == "\r\n" {
                    return true;
                }
                let mut it = rest.chars();
                match (it.next(), it.next()) {
                    (Some(c), None) if is_terminator(c, false) => {
                        !(c == '\n' && char_before(text, pos) == Some('\r'))
                    }
                    _ => false,
                }
            }
            Look::Boundary { unicode, negate } => {
                let is_word = |c: char| {
                    if unicode {
                        is_unicode_word(c)
                    } else {
                        is_ascii_word(c)
                    }
                };
                let nsm_word = |at: usize| -> bool {
                    // A non-spacing mark counts as a word character after a letter or digit.
                    let mut p = at;
                    while let Some(c) = char_at(text, p) {
                        if crate::unicode::is_letter_or_digit(c) {
                            return true;
                        }
                        if crate::unicode::get_type(c) != crate::unicode::gc::NON_SPACING_MARK {
                            return false;
                        }
                        match char_before(text, p) {
                            Some(b) => p -= b.len_utf8(),
                            None => return false,
                        }
                    }
                    false
                };
                let left = char_before(text, pos).is_some_and(|c| {
                    is_word(c)
                        || (crate::unicode::get_type(c) == crate::unicode::gc::NON_SPACING_MARK
                            && nsm_word(pos - c.len_utf8()))
                });
                let right = char_at(text, pos).is_some_and(|c| {
                    is_word(c)
                        || (crate::unicode::get_type(c) == crate::unicode::gc::NON_SPACING_MARK
                            && nsm_word(pos))
                });
                (left != right) != negate
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The parser.

#[derive(Clone, Copy, Debug)]
struct Flags {
    ci: bool,
    unicode_case: bool,
    dotall: bool,
    multiline: bool,
    comments: bool,
    unix_lines: bool,
    unicode_class: bool,
}

impl Flags {
    fn from(f: u32) -> Flags {
        Flags {
            ci: f & CASE_INSENSITIVE != 0,
            // UNICODE_CHARACTER_CLASS implies UNICODE_CASE.
            unicode_case: f & (UNICODE_CASE | UNICODE_CHARACTER_CLASS) != 0,
            dotall: f & DOTALL != 0,
            multiline: f & MULTILINE != 0,
            comments: f & COMMENTS != 0,
            unix_lines: f & UNIX_LINES != 0,
            unicode_class: f & UNICODE_CHARACTER_CLASS != 0,
        }
    }

    /// The set a literal character matches under these flags (Java's `single`): with
    /// CASE_INSENSITIVE|UNICODE_CASE every character whose `toLowerCase(toUpperCase(ch))` is
    /// that of `c`; with CASE_INSENSITIVE alone the ASCII case pair.
    fn char_set(&self, c: char) -> Ranges {
        let cp = c as u32;
        if !self.ci {
            return vec![(cp, cp)];
        }
        if self.unicode_case {
            let upper = crate::unicode::to_upper(c);
            let lower = crate::unicode::to_lower(upper);
            if upper != lower || cp == 0xDF {
                let lower = lower as u32;
                let mut v = vec![(cp, cp), (lower, lower)];
                v.extend(
                    cased_chars()
                        .iter()
                        .filter(|&&(_, _, l)| l == lower)
                        .map(|&(ch, _, _)| (ch, ch)),
                );
                return dfa::ranges(v);
            }
            return vec![(cp, cp)];
        }
        if c.is_ascii_alphabetic() {
            let (l, u) = (c.to_ascii_lowercase() as u32, c.to_ascii_uppercase() as u32);
            return dfa::ranges(vec![(l, l), (u, u)]);
        }
        vec![(cp, cp)]
    }

    /// The set a single character inside `[...]` matches: Latin-1 characters go into Java's
    /// `BitClass` (with both cases when case-insensitive) except the few whose case partner
    /// lies outside Latin-1, which are matched like literals.
    fn class_char_set(&self, c: char) -> Ranges {
        let cp = c as u32;
        let special = self.ci
            && self.unicode_case
            && matches!(
                cp,
                0xFF | 0xB5 | 0x49 | 0x69 | 0x53 | 0x73 | 0x4B | 0x6B | 0xC5 | 0xE5 | 0xDF
            );
        if cp >= 0x100 || special {
            return self.char_set(c);
        }
        let mut v = vec![(cp, cp)];
        if self.ci {
            if c.is_ascii() {
                for x in [c.to_ascii_uppercase(), c.to_ascii_lowercase()] {
                    v.push((x as u32, x as u32));
                }
            } else if self.unicode_case {
                for x in [crate::unicode::to_lower(c), crate::unicode::to_upper(c)] {
                    v.push((x as u32, x as u32));
                }
            }
        }
        dfa::ranges(v)
    }

    fn literal(&self, c: char) -> Node {
        let set = self.char_set(c);
        if set.len() == 1 && set[0].0 == set[0].1 {
            let mut s = String::new();
            s.push(c);
            Node::Str(s.into_boxed_str())
        } else {
            Node::Set(set.into())
        }
    }
}

/// Every code point with a simple case mapping: (ch, `toUpperCase(ch)`,
/// `toLowerCase(toUpperCase(ch))`).
fn cased_chars() -> &'static [(u32, u32, u32)] {
    static T: OnceLock<Vec<(u32, u32, u32)>> = OnceLock::new();
    T.get_or_init(|| {
        let mut v = Vec::new();
        for cp in 0..=0x10FFFFu32 {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            let u = crate::unicode::to_upper(c);
            let l = crate::unicode::to_lower(u);
            if u != c || l != c {
                v.push((cp, u as u32, l as u32));
            }
        }
        v
    })
}

/// Java's `CaseFolding` expanded case map: the C/S/T foldings that `toUpperCase` and
/// `toLowerCase` of the folding do not lead back from, plus `ı` → `I`.
fn expanded_case_map() -> &'static [(u32, u32)] {
    static T: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    T.get_or_init(|| {
        let mut v = vec![(0x0131, 0x49)];
        for &(cp, f) in crate::unicode::case_folding::CASE_FOLDING_CST {
            let Some(fc) = char::from_u32(f) else {
                continue;
            };
            if crate::unicode::to_upper(fc) as u32 != cp
                && crate::unicode::to_lower(fc) as u32 != cp
            {
                v.push((cp, f));
            }
        }
        v
    })
}

/// A syntax error: Java's description and index.
struct SyntaxError {
    desc: String,
    index: isize,
}

impl SyntaxError {
    /// `PatternSyntaxException.getMessage()`.
    fn into_error(self, pattern: &str) -> AnalysisError {
        let mut m = self.desc;
        if self.index >= 0 {
            m.push_str(&format!(" near index {}", self.index));
        }
        m.push('\n');
        m.push_str(pattern);
        let len = pattern.encode_utf16().count() as isize;
        if self.index >= 0 && self.index < len {
            m.push('\n');
            for _ in 0..self.index {
                m.push(' ');
            }
            m.push('^');
        }
        AnalysisError::other("pattern_syntax_exception", 400, m)
    }
}

fn err<T>(desc: impl Into<String>, index: usize) -> std::result::Result<T, SyntaxError> {
    Err(SyntaxError {
        desc: desc.into(),
        index: index as isize,
    })
}

type PResult<T> = std::result::Result<T, SyntaxError>;

struct Parser {
    chars: Vec<char>,
    /// UTF-16 index of each char (for error positions), plus the total at the end.
    idx16: Vec<usize>,
    i: usize,
    flags: Flags,
    groups: usize,
    names: Vec<(String, usize)>,
}

const ASCII_SPACE: &[(u32, u32)] = &[(0x09, 0x0D), (0x20, 0x20)];

impl Parser {
    fn new(src: &str, flags: u32) -> Parser {
        let chars: Vec<char> = src.chars().collect();
        let mut idx16 = Vec::with_capacity(chars.len() + 1);
        let mut n = 0;
        for c in &chars {
            idx16.push(n);
            n += c.len_utf16();
        }
        idx16.push(n);
        Parser {
            chars,
            idx16,
            i: 0,
            flags: Flags::from(flags),
            groups: 0,
            names: Vec::new(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.chars.get(self.i + off).copied()
    }

    /// UTF-16 index of char index `i` (clamped to the end).
    fn at(&self, i: usize) -> usize {
        self.idx16[i.min(self.chars.len())]
    }

    fn len16(&self) -> usize {
        self.idx16[self.chars.len()]
    }

    fn parse(&mut self) -> PResult<Node> {
        let n = self.alternation()?;
        if self.i < self.chars.len() {
            // Only an unmatched ')' stops the top level.
            return err("Unmatched closing ')'", self.at(self.i).saturating_sub(1));
        }
        Ok(n)
    }

    fn skip_comments(&mut self) {
        if !self.flags.comments {
            return;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii() && dfa::contains(ASCII_SPACE, c as u32) {
                self.i += 1;
            } else if c == '#' {
                while let Some(d) = self.peek() {
                    self.i += 1;
                    if d == '\n' || d == '\r' || d == '\u{85}' || d == '\u{2028}' || d == '\u{2029}'
                    {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn alternation(&mut self) -> PResult<Node> {
        let mut alts = vec![self.sequence()?];
        while self.peek() == Some('|') {
            self.i += 1;
            alts.push(self.sequence()?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().unwrap_or(Node::Empty)
        } else {
            Node::Alt(alts)
        })
    }

    fn sequence(&mut self) -> PResult<Node> {
        let mut items: Vec<Node> = Vec::new();
        loop {
            self.skip_comments();
            let Some(c) = self.peek() else { break };
            if c == '|' || c == ')' {
                break;
            }
            let atom = match self.atom()? {
                Some(a) => a,
                None => continue,
            };
            let atom = self.quantifier(atom)?;
            items.push(atom);
        }
        merge_literals(&mut items);
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Seq(items),
        })
    }

    /// Parses one atom; None for constructs that produce nothing (inline flags, `\Q\E`).
    fn atom(&mut self) -> PResult<Option<Node>> {
        let start = self.i;
        let c = self.chars[self.i];
        self.i += 1;
        Ok(Some(match c {
            '(' => return self.group(),
            '[' => Node::Set(self.class()?.into()),
            '.' => {
                let set = if self.flags.dotall {
                    vec![(0, 0x10FFFF)]
                } else if self.flags.unix_lines {
                    dfa::negate(&[(0x0A, 0x0A)])
                } else {
                    dfa::negate(&dfa::chars("\n\r\u{85}\u{2028}\u{2029}"))
                };
                Node::Set(set.into())
            }
            '^' => Node::Look(if self.flags.multiline {
                Look::StartLine {
                    unix: self.flags.unix_lines,
                }
            } else {
                Look::Start
            }),
            '$' => Node::Look(if self.flags.multiline {
                Look::EndLine {
                    unix: self.flags.unix_lines,
                }
            } else {
                Look::EndOrFinalTerminator {
                    unix: self.flags.unix_lines,
                }
            }),
            '*' | '+' | '?' => {
                return err(format!("Dangling meta character '{c}'"), self.at(start));
            }
            '{' => {
                // Java: a `{` that does not start a valid repetition here is an error.
                return err("Illegal repetition", self.at(start));
            }
            '\\' => return self.escape_atom(start),
            c => self.flags.literal(c),
        }))
    }

    fn quantifier(&mut self, atom: Node) -> PResult<Node> {
        self.skip_comments();
        let Some(c) = self.peek() else {
            return Ok(atom);
        };
        let (min, max) = match c {
            '*' => (0, None),
            '+' => (1, None),
            '?' => (0, Some(1)),
            '{' => {
                let open = self.i;
                let mut j = self.i + 1;
                let num = |p: &Parser, j: &mut usize| -> Option<u64> {
                    let s = *j;
                    let mut v: u64 = 0;
                    while let Some(d) = p.chars.get(*j).and_then(|c| c.to_digit(10)) {
                        v = (v * 10 + d as u64).min(u64::from(u32::MAX) + 1);
                        *j += 1;
                    }
                    (*j > s).then_some(v)
                };
                let Some(lo) = num(self, &mut j) else {
                    return err("Illegal repetition", self.at(open + 1));
                };
                let hi = if self.chars.get(j) == Some(&',') {
                    j += 1;
                    num(self, &mut j)
                } else {
                    Some(lo)
                };
                if self.chars.get(j) != Some(&'}') {
                    return err("Unclosed counted closure", self.at(j));
                }
                if lo > i32::MAX as u64 || hi.is_some_and(|h| h > i32::MAX as u64) {
                    return err("Illegal repetition range", self.at(j));
                }
                if hi.is_some_and(|h| h < lo) {
                    return err("Illegal repetition range", self.at(j));
                }
                self.i = j;
                (lo as u32, hi.map(|h| h as u32))
            }
            _ => return Ok(atom),
        };
        self.i += 1;
        let greed = match self.peek() {
            Some('?') => {
                self.i += 1;
                Greed::Lazy
            }
            Some('+') => {
                self.i += 1;
                Greed::Possessive
            }
            _ => Greed::Greedy,
        };
        // A quantified quantifier (`a**`) is a dangling meta character.
        if let Some(n @ ('*' | '+' | '?')) = self.peek() {
            return err(format!("Dangling meta character '{n}'"), self.at(self.i));
        }
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greed,
        })
    }

    fn group(&mut self) -> PResult<Option<Node>> {
        let saved = self.flags;
        let node = if self.peek() == Some('?') {
            self.i += 1;
            match self.peek() {
                Some(':') => {
                    self.i += 1;
                    Node::Group(Box::new(self.group_body()?), None)
                }
                Some('>') => {
                    self.i += 1;
                    Node::Atomic(Box::new(self.group_body()?))
                }
                Some('=') | Some('!') => {
                    let negate = self.peek() == Some('!');
                    self.i += 1;
                    Node::Around {
                        node: Box::new(self.group_body()?),
                        behind: false,
                        negate,
                    }
                }
                Some('<') => {
                    self.i += 1;
                    match self.peek() {
                        Some('=') | Some('!') => {
                            let negate = self.peek() == Some('!');
                            self.i += 1;
                            Node::Around {
                                node: Box::new(self.group_body()?),
                                behind: true,
                                negate,
                            }
                        }
                        _ => {
                            let name = self.group_name()?;
                            if self.names.iter().any(|(n, _)| *n == name) {
                                return err(
                                    format!("Named capturing group <{name}> is already defined"),
                                    self.at(self.i - 1),
                                );
                            }
                            self.groups += 1;
                            let g = self.groups;
                            self.names.push((name, g));
                            Node::Group(Box::new(self.group_body()?), Some(g))
                        }
                    }
                }
                _ => {
                    // Inline flags: (?idmsuxU-idmsuxU) or (?flags:X).
                    let mut on = true;
                    let mut f = self.flags;
                    loop {
                        let Some(c) = self.peek() else {
                            return err("Unknown inline modifier", self.at(self.i));
                        };
                        self.i += 1;
                        match c {
                            'i' => f.ci = on,
                            'u' => f.unicode_case = on,
                            'd' => f.unix_lines = on,
                            'm' => f.multiline = on,
                            's' => f.dotall = on,
                            'x' => f.comments = on,
                            'U' => {
                                f.unicode_class = on;
                                if on {
                                    f.unicode_case = true;
                                }
                            }
                            'c' => {}
                            '-' if on => on = false,
                            ')' => {
                                // Applies to the rest of the enclosing group.
                                self.flags = f;
                                return Ok(None);
                            }
                            ':' => {
                                self.flags = f;
                                let body = self.group_body()?;
                                self.flags = saved;
                                return Ok(Some(Node::Group(Box::new(body), None)));
                            }
                            _ => return err("Unknown inline modifier", self.at(self.i - 1)),
                        }
                    }
                }
            }
        } else {
            self.groups += 1;
            let g = self.groups;
            Node::Group(Box::new(self.group_body()?), Some(g))
        };
        self.flags = saved;
        Ok(Some(node))
    }

    fn group_name(&mut self) -> PResult<String> {
        let mut name = String::new();
        match self.peek() {
            Some(c) if c.is_ascii_alphabetic() => {}
            _ => {
                return err(
                    "capturing group name does not start with a Latin letter",
                    self.at(self.i),
                );
            }
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() {
                name.push(c);
                self.i += 1;
            } else {
                break;
            }
        }
        if self.peek() != Some('>') {
            return err(
                "named capturing group is missing trailing '>'",
                self.at(self.i),
            );
        }
        self.i += 1;
        Ok(name)
    }

    fn group_body(&mut self) -> PResult<Node> {
        let body = self.alternation()?;
        if self.peek() != Some(')') {
            return err("Unclosed group", self.len16());
        }
        self.i += 1;
        Ok(body)
    }

    /// An escape outside a class. `start` is the index of the backslash.
    fn escape_atom(&mut self, start: usize) -> PResult<Option<Node>> {
        let Some(c) = self.peek() else {
            return err("Unescaped trailing backslash", self.len16());
        };
        match c {
            'b' | 'B' => {
                self.i += 1;
                if self.peek() == Some('{') {
                    // `\b{g}` is the only braced boundary Java has.
                    if self.chars.get(self.i + 1) == Some(&'g')
                        && self.chars.get(self.i + 2) == Some(&'}')
                    {
                        self.i += 3;
                        return Ok(Some(Node::Look(Look::Grapheme)));
                    }
                    return err("Illegal/unsupported escape sequence", self.at(self.i));
                }
                Ok(Some(Node::Look(Look::Boundary {
                    unicode: self.flags.unicode_class,
                    negate: c == 'B',
                })))
            }
            'A' => {
                self.i += 1;
                Ok(Some(Node::Look(Look::Start)))
            }
            'z' => {
                self.i += 1;
                Ok(Some(Node::Look(Look::End)))
            }
            'Z' => {
                self.i += 1;
                Ok(Some(Node::Look(Look::EndOrFinalTerminator {
                    unix: self.flags.unix_lines,
                })))
            }
            'G' => {
                self.i += 1;
                Ok(Some(Node::Look(Look::SearchStart)))
            }
            'R' => {
                self.i += 1;
                // (?>\r\n|[\n\x0B\f\r\x85\u2028\u2029])
                Ok(Some(Node::Atomic(Box::new(Node::Alt(vec![
                    Node::Str("\r\n".into()),
                    Node::Set(dfa::chars("\n\x0B\x0C\r\u{85}\u{2028}\u{2029}").into()),
                ])))))
            }
            'X' => {
                self.i += 1;
                Ok(Some(Node::Grapheme))
            }
            'Q' => {
                self.i += 1;
                let mut items = Vec::new();
                while let Some(d) = self.peek() {
                    if d == '\\' && self.peek_at(1) == Some('E') {
                        self.i += 2;
                        break;
                    }
                    self.i += 1;
                    items.push(self.flags.literal(d));
                }
                merge_literals(&mut items);
                Ok(Some(match items.len() {
                    0 => return Ok(None),
                    1 => items.pop().unwrap_or(Node::Empty),
                    _ => Node::Seq(items),
                }))
            }
            'E' => {
                // A stray \E is ignored.
                self.i += 1;
                Ok(None)
            }
            '1'..='9' => {
                self.i += 1;
                let mut g = c.to_digit(10).unwrap_or(0) as usize;
                while let Some(d) = self.peek().and_then(|d| d.to_digit(10)) {
                    let ng = g * 10 + d as usize;
                    if ng > self.groups {
                        break;
                    }
                    g = ng;
                    self.i += 1;
                }
                Ok(Some(Node::Backref {
                    group: g,
                    ci: self.flags.ci,
                    unicode_case: self.flags.unicode_case,
                }))
            }
            'k' => {
                self.i += 1;
                if self.peek() != Some('<') {
                    return err(
                        "\\k is not followed by '<' for named capturing group",
                        self.at(self.i),
                    );
                }
                self.i += 1;
                let name = self.group_name()?;
                match self.names.iter().find(|(n, _)| *n == name) {
                    Some(&(_, g)) => Ok(Some(Node::Backref {
                        group: g,
                        ci: self.flags.ci,
                        unicode_case: self.flags.unicode_case,
                    })),
                    None => err(
                        format!("named capturing group <{name}> does not exist"),
                        self.at(self.i - 1),
                    ),
                }
            }
            _ => {
                let set = self.escape_set(start, false)?;
                Ok(Some(set_node(set)))
            }
        }
    }

    /// An escape that denotes a set of characters (in or outside a class).
    fn escape_set(&mut self, _start: usize, _in_class: bool) -> PResult<Ranges> {
        let Some(c) = self.peek() else {
            return err("Unescaped trailing backslash", self.len16());
        };
        self.i += 1;
        if let Some(set) = self.predefined(c) {
            return Ok(set);
        }
        match c {
            'p' | 'P' => {
                let set = self.property()?;
                return Ok(if c == 'P' { dfa::negate(&set) } else { set });
            }
            'N' => return err("\\N{name} is not supported", self.at(self.i - 1)),
            _ => {}
        }
        let ch = self.escaped_char(c)?;
        Ok(self.flags.char_set(ch))
    }

    /// The character an escape like `\t`, `\x41`, `\u0041`, `\0101`, `\cA` or `\.` denotes.
    fn escaped_char(&mut self, c: char) -> PResult<char> {
        Ok(match c {
            't' => '\t',
            'n' => '\n',
            'r' => '\r',
            'f' => '\x0C',
            'a' => '\x07',
            'e' => '\x1B',
            '0' => {
                // Octal: \0n, \0nn, \0mnn (m <= 3).
                let d1 = self.peek().and_then(|d| d.to_digit(8));
                let Some(d1) = d1 else {
                    return err("Illegal octal escape sequence", self.at(self.i));
                };
                self.i += 1;
                let mut v = d1;
                if let Some(d2) = self.peek().and_then(|d| d.to_digit(8)) {
                    self.i += 1;
                    v = v * 8 + d2;
                    if d1 <= 3
                        && let Some(d3) = self.peek().and_then(|d| d.to_digit(8))
                    {
                        self.i += 1;
                        v = v * 8 + d3;
                    }
                }
                char::from_u32(v).unwrap_or('\0')
            }
            'x' => {
                if self.peek() == Some('{') {
                    self.i += 1;
                    let mut v: u64 = 0;
                    let mut n = 0;
                    while let Some(d) = self.peek().and_then(|d| d.to_digit(16)) {
                        v = (v * 16 + d as u64).min(0x1_0000_0000);
                        self.i += 1;
                        n += 1;
                    }
                    if n == 0 || self.peek() != Some('}') {
                        return err("Unclosed hexadecimal escape sequence", self.at(self.i));
                    }
                    if v > 0x10FFFF {
                        return err("Hexadecimal codepoint is too big", self.at(self.i - 1));
                    }
                    self.i += 1;
                    char::from_u32(v as u32).unwrap_or('\u{FFFD}')
                } else {
                    let mut v = 0;
                    for _ in 0..2 {
                        let Some(d) = self.peek().and_then(|d| d.to_digit(16)) else {
                            return err("Illegal hexadecimal escape sequence", self.at(self.i));
                        };
                        v = v * 16 + d;
                        self.i += 1;
                    }
                    char::from_u32(v).unwrap_or('\0')
                }
            }
            'u' => {
                let hi = self.hex4()?;
                if (0xD800..0xDC00).contains(&hi)
                    && self.peek() == Some('\\')
                    && self.peek_at(1) == Some('u')
                {
                    let save = self.i;
                    self.i += 2;
                    let lo = self.hex4()?;
                    if (0xDC00..0xE000).contains(&lo) {
                        return Ok(
                            char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                                .unwrap_or('\u{FFFD}'),
                        );
                    }
                    self.i = save;
                }
                char::from_u32(hi).unwrap_or('\u{FFFD}')
            }
            'c' => {
                let Some(d) = self.peek() else {
                    return err(
                        "Illegal control escape sequence",
                        self.at(self.i).saturating_sub(1),
                    );
                };
                self.i += 1;
                char::from_u32(d as u32 ^ 64).unwrap_or('\0')
            }
            c if c.is_ascii_alphabetic() => {
                return err("Illegal/unsupported escape sequence", self.at(self.i - 1));
            }
            c => c,
        })
    }

    fn hex4(&mut self) -> PResult<u32> {
        let mut v = 0;
        for _ in 0..4 {
            let Some(d) = self.peek().and_then(|d| d.to_digit(16)) else {
                return err("Illegal Unicode escape sequence", self.at(self.i));
            };
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    /// `\d \D \w \W \s \S \h \H \v \V`.
    fn predefined(&self, c: char) -> Option<Ranges> {
        let u = self.flags.unicode_class;
        let (set, neg): (Ranges, bool) = match c.to_ascii_lowercase() {
            'd' => (
                if u {
                    unicode_prop("Nd")
                } else {
                    vec![(0x30, 0x39)]
                },
                c == 'D',
            ),
            'w' => (
                if u {
                    unicode_word()
                } else {
                    vec![(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)]
                },
                c == 'W',
            ),
            's' => (
                if u {
                    unicode_prop("White_Space")
                } else {
                    ASCII_SPACE.to_vec()
                },
                c == 'S',
            ),
            'h' => (
                dfa::ranges(vec![
                    (0x20, 0x20),
                    (0x09, 0x09),
                    (0xA0, 0xA0),
                    (0x1680, 0x1680),
                    (0x180E, 0x180E),
                    (0x2000, 0x200A),
                    (0x202F, 0x202F),
                    (0x205F, 0x205F),
                    (0x3000, 0x3000),
                ]),
                c == 'H',
            ),
            'v' => (
                dfa::ranges(vec![(0x0A, 0x0D), (0x85, 0x85), (0x2028, 0x2029)]),
                c == 'V',
            ),
            _ => return None,
        };
        let set = dfa::ranges(set);
        Some(if neg { dfa::negate(&set) } else { set })
    }

    /// `\p{…}` / `\pL` (after the `p`).
    fn property(&mut self) -> PResult<Ranges> {
        let name: String = if self.peek() == Some('{') {
            self.i += 1;
            let start = self.i;
            while let Some(c) = self.peek() {
                if c == '}' {
                    break;
                }
                self.i += 1;
            }
            if self.peek() != Some('}') {
                return err("Unclosed character family", self.len16());
            }
            let name: String = self.chars[start..self.i].iter().collect();
            self.i += 1;
            if name.is_empty() {
                return err("Empty character family", self.at(self.i - 1));
            }
            name
        } else {
            match self.peek() {
                Some(c) => {
                    self.i += 1;
                    c.to_string()
                }
                None => return err("Illegal character family", self.len16()),
            }
        };
        if let Some(eq) = name.find('=') {
            // `\p{name=value}`: Java lower-cases the property name and knows three.
            let key = name[..eq].to_lowercase();
            let value = &name[eq + 1..];
            let set = match key.as_str() {
                "sc" | "script" => script(value),
                "blk" | "block" => block(value),
                "gc" | "general_category" => resolve_property(
                    value,
                    Flags {
                        unicode_class: false,
                        ..self.flags
                    },
                ),
                _ => None,
            };
            return set.map_or_else(
                || {
                    err(
                        format!("Unknown Unicode property {{name=<{key}>, value=<{value}>}}"),
                        self.at(self.i - 1),
                    )
                },
                Ok,
            );
        }
        let unknown = |p: &Parser| {
            err(
                format!("Unknown character property name {{{name}}}"),
                p.at(p.i - 1),
            )
        };
        let set = match resolve_property(&name, self.flags) {
            Some(s) => s,
            None => return unknown(self),
        };
        Ok(set)
    }

    /// A character class (after the `[`).
    fn class(&mut self) -> PResult<Ranges> {
        let open = self.i - 1;
        let negate = if self.peek() == Some('^') {
            self.i += 1;
            true
        } else {
            false
        };
        // Intersection operands: each is a union of items.
        let mut operands: Vec<Ranges> = Vec::new();
        let mut cur: Ranges = Vec::new();
        let mut first = true;
        loop {
            if self.flags.comments {
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii() && dfa::contains(ASCII_SPACE, c as u32))
                {
                    self.i += 1;
                }
            }
            let Some(c) = self.peek() else {
                let _ = open;
                return err("Unclosed character class", self.len16().saturating_sub(1));
            };
            match c {
                ']' if !first || operands.is_empty() && !cur.is_empty() => {
                    self.i += 1;
                    break;
                }
                ']' => {
                    // `[]` and `[^]`: Java has no empty class; `]` first is still the end, so the
                    // class is unclosed unless more follows.
                    self.i += 1;
                    if self.peek().is_none() {
                        return err("Unclosed character class", self.len16().saturating_sub(1));
                    }
                    // Java treats a leading `]` as the closing bracket of an empty class only at the
                    // very end; otherwise it is literal.
                    cur.extend(self.flags.class_char_set(']'));
                }
                '[' => {
                    self.i += 1;
                    let inner = self.class()?;
                    cur.extend(inner);
                }
                '&' if self.peek_at(1) == Some('&') => {
                    self.i += 2;
                    operands.push(dfa::ranges(std::mem::take(&mut cur)));
                }
                _ => {
                    let lo_set = self.class_atom()?;
                    // A range `a-b` (a `-` followed by `]` or `[` is literal).
                    if let Some(lo) = lo_set.1
                        && self.peek() == Some('-')
                        && !matches!(self.peek_at(1), Some(']') | Some('[') | None)
                    {
                        self.i += 1;
                        let (_, hi) = self.class_atom()?;
                        let Some(hi) = hi else {
                            return err("Illegal character range", self.at(self.i - 1));
                        };
                        if hi < lo {
                            return err("Illegal character range", self.at(self.i - 1));
                        }
                        cur.extend(self.range_set(lo, hi));
                    } else {
                        cur.extend(lo_set.0);
                    }
                }
            }
            first = false;
        }
        let mut set = dfa::ranges(cur);
        for op in operands.into_iter().rev() {
            set = dfa::intersect(&op, &set);
        }
        Ok(if negate { dfa::negate(&set) } else { set })
    }

    /// One class item: (its set, the character if it is a single character usable in a range).
    fn class_atom(&mut self) -> PResult<(Ranges, Option<char>)> {
        let c = self.peek().unwrap_or('\0');
        self.i += 1;
        if c != '\\' {
            return Ok((self.flags.class_char_set(c), Some(c)));
        }
        let Some(e) = self.peek() else {
            return err("Unescaped trailing backslash", self.len16());
        };
        if e == 'Q' {
            // \Q…\E inside a class: each quoted character is an item.
            self.i += 1;
            let mut set = Vec::new();
            while let Some(d) = self.peek() {
                if d == '\\' && self.peek_at(1) == Some('E') {
                    self.i += 2;
                    break;
                }
                self.i += 1;
                set.extend(self.flags.class_char_set(d));
            }
            return Ok((set, None));
        }
        if self.predefined(e).is_some() || e == 'p' || e == 'P' || e == 'N' {
            let set = self.escape_set(self.i - 1, true)?;
            return Ok((set, None));
        }
        self.i += 1;
        let ch = self.escaped_char(e)?;
        Ok((self.flags.class_char_set(ch), Some(ch)))
    }

    fn range_set(&self, lo: char, hi: char) -> Ranges {
        let mut v = vec![(lo as u32, hi as u32)];
        if self.flags.ci {
            if self.flags.unicode_case {
                // Java's `CIRangeU`: `ch` matches when it, its upper case or the lower case of
                // that is in the range, or when either case is one of the range's closing
                // characters (foldings of members that lie outside it).
                let (l, h) = (lo as u32, hi as u32);
                let closing: Vec<u32> = expanded_case_map()
                    .iter()
                    .filter(|&&(cp, f)| (l..=h).contains(&cp) && !(l..=h).contains(&f))
                    .map(|&(_, f)| f)
                    .collect();
                let hit = |x: u32| (l..=h).contains(&x) || closing.contains(&x);
                for &(ch, u, lw) in cased_chars() {
                    if hit(u) || hit(lw) {
                        v.push((ch, ch));
                    }
                }
                for &c in &closing {
                    v.push((c, c));
                }
            } else {
                for (a, z, delta) in [('a', 'z', -32i32), ('A', 'Z', 32)] {
                    let l = (lo as u32).max(a as u32);
                    let h = (hi as u32).min(z as u32);
                    if l <= h {
                        v.push(((l as i32 + delta) as u32, (h as i32 + delta) as u32));
                    }
                }
            }
        }
        dfa::ranges(v)
    }
}

fn set_node(set: Ranges) -> Node {
    if set.len() == 1
        && set[0].0 == set[0].1
        && let Some(c) = char::from_u32(set[0].0)
    {
        return Node::Str(c.to_string().into_boxed_str());
    }
    Node::Set(set.into())
}

/// Joins adjacent exact strings into one.
fn merge_literals(items: &mut Vec<Node>) {
    let mut out: Vec<Node> = Vec::with_capacity(items.len());
    for n in items.drain(..) {
        if let (Some(Node::Str(prev)), Node::Str(s)) = (out.last_mut(), &n) {
            let mut joined = String::with_capacity(prev.len() + s.len());
            joined.push_str(prev);
            joined.push_str(s);
            *prev = joined.into_boxed_str();
            continue;
        }
        out.push(n);
    }
    *items = out;
}

// ---------------------------------------------------------------------------------------------
// Unicode properties.

/// A Unicode property with the JDK's data (`\p{name}`). General categories and the binary
/// properties Java derives from them come from our Unicode 17 tables (`crate::unicode`), as in
/// the JDK Elasticsearch bundles; the few others (`Bidi_Mirrored`, the emoji properties) fall
/// back to `regex-syntax`'s tables.
fn unicode_prop(name: &str) -> Ranges {
    use crate::unicode::{self, gc};
    fn cat(mask: u32) -> impl Fn(char) -> bool {
        move |c| mask & (1 << unicode::get_type(c)) != 0
    }
    const L: u32 = (1 << gc::UPPERCASE_LETTER)
        | (1 << gc::LOWERCASE_LETTER)
        | (1 << gc::TITLECASE_LETTER)
        | (1 << gc::MODIFIER_LETTER)
        | (1 << gc::OTHER_LETTER);
    const M: u32 =
        (1 << gc::NON_SPACING_MARK) | (1 << gc::ENCLOSING_MARK) | (1 << gc::COMBINING_SPACING_MARK);
    const P: u32 = (1 << gc::DASH_PUNCTUATION)
        | (1 << gc::START_PUNCTUATION)
        | (1 << gc::END_PUNCTUATION)
        | (1 << gc::CONNECTOR_PUNCTUATION)
        | (1 << gc::OTHER_PUNCTUATION)
        | (1 << gc::INITIAL_QUOTE_PUNCTUATION)
        | (1 << gc::FINAL_QUOTE_PUNCTUATION);
    const Z: u32 =
        (1 << gc::SPACE_SEPARATOR) | (1 << gc::LINE_SEPARATOR) | (1 << gc::PARAGRAPH_SEPARATOR);
    let mask = match name {
        "L" => L,
        "M" => M,
        "P" => P,
        "Z" => Z,
        "Lu" => 1 << gc::UPPERCASE_LETTER,
        "Ll" => 1 << gc::LOWERCASE_LETTER,
        "Lt" => 1 << gc::TITLECASE_LETTER,
        "Lm" => 1 << gc::MODIFIER_LETTER,
        "Lo" => 1 << gc::OTHER_LETTER,
        "Mn" => 1 << gc::NON_SPACING_MARK,
        "Me" => 1 << gc::ENCLOSING_MARK,
        "Mc" => 1 << gc::COMBINING_SPACING_MARK,
        "Nd" => 1 << gc::DECIMAL_DIGIT_NUMBER,
        "Nl" => 1 << gc::LETTER_NUMBER,
        "No" => 1 << gc::OTHER_NUMBER,
        "Zs" => 1 << gc::SPACE_SEPARATOR,
        "Cc" => 1 << gc::CONTROL,
        "Cf" => 1 << gc::FORMAT,
        "Cn" => 1 << gc::UNASSIGNED,
        "Pc" => 1 << gc::CONNECTOR_PUNCTUATION,
        "Sc" => 1 << gc::CURRENCY_SYMBOL,
        _ => 0,
    };
    if mask != 0 {
        return java_pred_dyn(name, cat(mask));
    }
    match name {
        "Cs" => vec![(0xD800, 0xDFFF)],
        "Alphabetic" => java_pred("javaAlphabetic", unicode::is_alphabetic),
        "Lowercase" => java_pred("javaLowerCase", unicode::is_lowercase),
        "Uppercase" => java_pred("javaUpperCase", unicode::is_uppercase),
        // `UnicodeProp.WHITE_SPACE`: the separators, \t..\r and NEL.
        "White_Space" => java_pred_dyn("WHITE_SPACE", |c| {
            Z & (1 << unicode::get_type(c)) != 0 || ('\t'..='\r').contains(&c) || c == '\u{85}'
        }),
        // `UnicodeProp.HEX_DIGIT`: decimal digits plus the ASCII and fullwidth letters A-F.
        "Hex_Digit" => java_pred_dyn("HEX_DIGIT", |c| {
            unicode::get_type(c) == gc::DECIMAL_DIGIT_NUMBER
                || c.is_ascii_hexdigit()
                || matches!(c, '\u{FF10}'..='\u{FF19}' | '\u{FF21}'..='\u{FF26}' | '\u{FF41}'..='\u{FF46}')
        }),
        "Join_Control" => vec![(0x200C, 0x200D)],
        _ => try_unicode_prop(name).unwrap_or_default(),
    }
}

/// [`java_pred`] for a predicate that is not a plain `fn` (cached under `key`).
fn java_pred_dyn(key: &str, f: impl Fn(char) -> bool) -> Ranges {
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Ranges>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
        return r.clone();
    }
    let mut v: Ranges = Vec::new();
    for cp in 0..=0x10FFFFu32 {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        if f(c) {
            match v.last_mut() {
                Some(last) if last.1 + 1 == cp => last.1 = cp,
                _ => v.push((cp, cp)),
            }
        }
    }
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string(), v.clone());
    v
}

fn try_unicode_prop(name: &str) -> Option<Ranges> {
    if name.is_empty() || name.contains(['}', '{', '\\', ']', '[']) {
        return None;
    }
    let hir = regex_syntax::ParserBuilder::new()
        .build()
        .parse(&format!("\\p{{{name}}}"))
        .ok()?;
    hir_ranges(&hir)
}

fn hir_ranges(hir: &regex_syntax::hir::Hir) -> Option<Ranges> {
    use regex_syntax::hir::{Class, HirKind};
    match hir.kind() {
        HirKind::Class(Class::Unicode(c)) => Some(
            c.ranges()
                .iter()
                .map(|r| (r.start() as u32, r.end() as u32))
                .collect(),
        ),
        HirKind::Literal(l) => {
            let s = std::str::from_utf8(&l.0).ok()?;
            Some(s.chars().map(|c| (c as u32, c as u32)).collect())
        }
        _ => None,
    }
}

/// `\w` with UNICODE_CHARACTER_CLASS: Alphabetic, marks, decimal digits, connector punctuation,
/// join controls.
fn unicode_word() -> Ranges {
    static W: OnceLock<Ranges> = OnceLock::new();
    W.get_or_init(|| {
        let mut v = Vec::new();
        for p in ["Alphabetic", "M", "Nd", "Pc", "Join_Control"] {
            v.extend(unicode_prop(p));
        }
        dfa::ranges(v)
    })
    .clone()
}

/// The code points a `java.lang.Character` predicate accepts.
fn java_pred(key: &'static str, f: fn(char) -> bool) -> Ranges {
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<&'static str, Ranges>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
        return r.clone();
    }
    let mut v: Ranges = Vec::new();
    for cp in 0..=0x10FFFFu32 {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        if f(c) {
            match v.last_mut() {
                Some(last) if last.1 + 1 == cp => last.1 = cp,
                _ => v.push((cp, cp)),
            }
        }
    }
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, v.clone());
    v
}

/// Resolves a `\p{…}` name with Java's rules (`CharPredicates.forProperty` and friends).
fn resolve_property(name: &str, f: Flags) -> Option<Ranges> {
    let u = f.unicode_class;
    let posix = |n: &str| -> Option<Ranges> {
        Some(match n {
            "Lower" => {
                if u || f.ci {
                    // With CASE_INSENSITIVE Java matches both cases for \p{Lower}/\p{Upper}.
                    if f.ci && !u {
                        vec![(0x41, 0x5A), (0x61, 0x7A)]
                    } else {
                        unicode_prop("Lowercase")
                    }
                } else {
                    vec![(0x61, 0x7A)]
                }
            }
            "Upper" => {
                if f.ci && !u {
                    vec![(0x41, 0x5A), (0x61, 0x7A)]
                } else if u {
                    unicode_prop("Uppercase")
                } else {
                    vec![(0x41, 0x5A)]
                }
            }
            "ASCII" => vec![(0, 0x7F)],
            "Alpha" => {
                if u {
                    unicode_prop("Alphabetic")
                } else {
                    vec![(0x41, 0x5A), (0x61, 0x7A)]
                }
            }
            "Digit" => {
                if u {
                    unicode_prop("Nd")
                } else {
                    vec![(0x30, 0x39)]
                }
            }
            "Alnum" => {
                if u {
                    dfa::union(&[&unicode_prop("Alphabetic"), &unicode_prop("Nd")])
                } else {
                    vec![(0x30, 0x39), (0x41, 0x5A), (0x61, 0x7A)]
                }
            }
            "Punct" => {
                if u {
                    unicode_prop("P")
                } else {
                    vec![(0x21, 0x2F), (0x3A, 0x40), (0x5B, 0x60), (0x7B, 0x7E)]
                }
            }
            "Graph" => {
                if u {
                    dfa::negate(&dfa::union(&[
                        &unicode_prop("White_Space"),
                        &unicode_prop("Cc"),
                        &unicode_prop("Cs"),
                        &unicode_prop("Cn"),
                    ]))
                } else {
                    vec![(0x21, 0x7E)]
                }
            }
            "Print" => {
                if u {
                    let graph = dfa::negate(&dfa::union(&[
                        &unicode_prop("White_Space"),
                        &unicode_prop("Cc"),
                        &unicode_prop("Cs"),
                        &unicode_prop("Cn"),
                    ]));
                    let blank = dfa::union(&[&unicode_prop("Zs"), &[(9, 9)]]);
                    dfa::difference(&dfa::union(&[&graph, &blank]), &unicode_prop("Cc"))
                } else {
                    vec![(0x20, 0x7E)]
                }
            }
            "Blank" => {
                if u {
                    dfa::union(&[&unicode_prop("Zs"), &[(9, 9)]])
                } else {
                    vec![(0x09, 0x09), (0x20, 0x20)]
                }
            }
            "Cntrl" => {
                if u {
                    unicode_prop("Cc")
                } else {
                    vec![(0x00, 0x1F), (0x7F, 0x7F)]
                }
            }
            "XDigit" => {
                if u {
                    dfa::union(&[&unicode_prop("Nd"), &unicode_prop("Hex_Digit")])
                } else {
                    vec![(0x30, 0x39), (0x41, 0x46), (0x61, 0x66)]
                }
            }
            "Space" => {
                if u {
                    unicode_prop("White_Space")
                } else {
                    ASCII_SPACE.to_vec()
                }
            }
            "javaLowerCase" | "javaUpperCase" | "javaTitleCase" if f.ci => dfa::union(&[
                &java_pred("javaLowerCase", crate::unicode::is_lowercase),
                &java_pred("javaUpperCase", crate::unicode::is_uppercase),
                &java_categories(&["Lt"]),
            ]),
            "javaLowerCase" => java_pred("javaLowerCase", crate::unicode::is_lowercase),
            "javaUpperCase" => java_pred("javaUpperCase", crate::unicode::is_uppercase),
            "javaAlphabetic" => java_pred("javaAlphabetic", crate::unicode::is_alphabetic),
            "javaIdeographic" => java_pred("javaIdeographic", crate::unicode::is_ideographic),
            "javaTitleCase" => java_categories(&["Lt"]),
            "javaDigit" => java_pred("javaDigit", crate::unicode::is_digit),
            "javaDefined" => dfa::negate(&java_categories(&["Cn"])),
            "javaLetter" => java_pred("javaLetter", crate::unicode::is_letter),
            "javaLetterOrDigit" => {
                java_pred("javaLetterOrDigit", crate::unicode::is_letter_or_digit)
            }
            "javaSpaceChar" => java_pred("javaSpaceChar", crate::unicode::is_space_char),
            "javaWhitespace" => java_pred("javaWhitespace", crate::unicode::is_whitespace),
            "javaISOControl" => vec![(0x00, 0x1F), (0x7F, 0x9F)],
            "javaMirrored" => unicode_prop("Bidi_Mirrored"),
            "javaIdentifierIgnorable" => dfa::union(&[
                &[(0x00, 0x08), (0x0E, 0x1B), (0x7F, 0x9F)],
                &unicode_prop("Cf"),
            ]),
            "javaJavaIdentifierStart" | "javaUnicodeIdentifierStart" => {
                let mut v = dfa::union(&[&unicode_prop("L"), &unicode_prop("Nl")]);
                if n == "javaJavaIdentifierStart" {
                    v = dfa::union(&[&v, &unicode_prop("Sc"), &unicode_prop("Pc")]);
                }
                v
            }
            "javaJavaIdentifierPart" | "javaUnicodeIdentifierPart" => {
                let mut v = dfa::union(&[
                    &unicode_prop("L"),
                    &unicode_prop("Nl"),
                    &unicode_prop("Nd"),
                    &unicode_prop("Mn"),
                    &unicode_prop("Mc"),
                    &unicode_prop("Pc"),
                    &unicode_prop("Cf"),
                    &[(0x00, 0x08), (0x0E, 0x1B), (0x7F, 0x9F)],
                ]);
                if n == "javaJavaIdentifierPart" {
                    v = dfa::union(&[&v, &unicode_prop("Sc")]);
                }
                v
            }
            "LC" => java_categories(&["Lu", "Ll", "Lt"]),
            "LD" => java_categories(&["Lu", "Ll", "Lt", "Lm", "Lo", "Nd"]),
            "L1" => vec![(0, 0xFF)],
            "all" => vec![(0, 0x10FFFF)],
            _ => return None,
        })
    };
    let category = |n: &str| -> Option<Ranges> {
        // `CharPredicates.forProperty`'s general categories, spelled exactly as Java does; with
        // CASE_INSENSITIVE the cased-letter categories match all three cases.
        if f.ci && matches!(n, "Lu" | "Ll" | "Lt") {
            return Some(java_categories(&["Lu", "Ll", "Lt"]));
        }
        let parts: &[&str] = match n {
            "L" => &["Lu", "Ll", "Lt", "Lm", "Lo"],
            "M" => &["Mn", "Me", "Mc"],
            "N" => &["Nd", "Nl", "No"],
            "Z" => &["Zs", "Zl", "Zp"],
            "C" => &["Cc", "Cf", "Co", "Cs", "Cn"],
            "P" => &["Pd", "Ps", "Pe", "Pc", "Po", "Pi", "Pf"],
            "S" => &["Sm", "Sc", "Sk", "So"],
            "Cn" | "Lu" | "Ll" | "Lt" | "Lm" | "Lo" | "Mn" | "Me" | "Mc" | "Nd" | "Nl" | "No"
            | "Zs" | "Zl" | "Zp" | "Cc" | "Cf" | "Co" | "Cs" | "Pd" | "Ps" | "Pe" | "Pc" | "Po"
            | "Sm" | "Sc" | "Sk" | "So" | "Pi" | "Pf" => std::slice::from_ref(&n),
            _ => return None,
        };
        Some(java_categories(parts))
    };
    if let Some(rest) = name.strip_prefix("Is") {
        // Binary properties, categories and scripts.
        if let Some(r) = binary_property(rest, f.ci) {
            return Some(r);
        }
        if let Some(r) = category(rest) {
            return Some(r);
        }
        return script(rest);
    }
    if let Some(rest) = name.strip_prefix("In") {
        return block(rest);
    }
    if let Some(r) = posix(name) {
        return Some(r);
    }
    category(name)
}

/// Java's `\p{IsX}` binary properties (`CharPredicates.forUnicodeProperty`: the name upper-cased,
/// then the POSIX names with their Unicode meaning). With CASE_INSENSITIVE the case properties
/// match all three cases.
fn binary_property(name: &str, ci: bool) -> Option<Ranges> {
    let cased = || {
        dfa::union(&[
            &java_pred("javaLowerCase", crate::unicode::is_lowercase),
            &java_pred("javaUpperCase", crate::unicode::is_uppercase),
            &unicode_prop("Lt"),
        ])
    };
    let n = name.to_uppercase();
    Some(match n.as_str() {
        "ALPHABETIC" | "ALPHA" => java_pred("javaAlphabetic", crate::unicode::is_alphabetic),
        "LETTER" => java_pred("javaLetter", crate::unicode::is_letter),
        "IDEOGRAPHIC" => java_pred("javaIdeographic", crate::unicode::is_ideographic),
        "LOWERCASE" | "LOWER" | "UPPERCASE" | "UPPER" | "TITLECASE" if ci => cased(),
        "LOWERCASE" | "LOWER" => java_pred("javaLowerCase", crate::unicode::is_lowercase),
        "UPPERCASE" | "UPPER" => java_pred("javaUpperCase", crate::unicode::is_uppercase),
        "TITLECASE" => unicode_prop("Lt"),
        "WHITE_SPACE" | "WHITESPACE" | "SPACE" => dfa::union(&[
            &java_categories(&["Zs", "Zl", "Zp"]),
            &[(0x09, 0x0D), (0x85, 0x85)],
        ]),
        "CONTROL" | "CNTRL" => java_categories(&["Cc"]),
        "PUNCTUATION" | "PUNCT" => java_categories(&["Pc", "Pd", "Ps", "Pe", "Po", "Pi", "Pf"]),
        "HEX_DIGIT" | "HEXDIGIT" | "XDIGIT" => dfa::union(&[
            &java_pred("javaDigit", crate::unicode::is_digit),
            &[
                (0x30, 0x39),
                (0x41, 0x46),
                (0x61, 0x66),
                (0xFF10, 0xFF19),
                (0xFF21, 0xFF26),
                (0xFF41, 0xFF46),
            ],
        ]),
        "ASSIGNED" => dfa::negate(&java_categories(&["Cn"])),
        "NONCHARACTER_CODE_POINT" | "NONCHARACTERCODEPOINT" => {
            let mut v = vec![(0xFDD0, 0xFDEF)];
            v.extend((0..=0x10u32).map(|p| ((p << 16) | 0xFFFE, (p << 16) | 0xFFFF)));
            dfa::ranges(v)
        }
        "DIGIT" => java_pred("javaDigit", crate::unicode::is_digit),
        "ALNUM" => dfa::union(&[
            &java_pred("javaAlphabetic", crate::unicode::is_alphabetic),
            &java_pred("javaDigit", crate::unicode::is_digit),
        ]),
        "BLANK" => dfa::union(&[&java_categories(&["Zs"]), &[(9, 9)]]),
        "GRAPH" => dfa::negate(&java_categories(&["Zs", "Zl", "Zp", "Cc", "Cs", "Cn"])),
        "PRINT" => {
            let graph = dfa::negate(&java_categories(&["Zs", "Zl", "Zp", "Cc", "Cs", "Cn"]));
            let blank = dfa::union(&[&java_categories(&["Zs"]), &[(9, 9)]]);
            dfa::difference(&dfa::union(&[&graph, &blank]), &java_categories(&["Cc"]))
        }
        "WORD" => unicode_word(),
        "JOIN_CONTROL" | "JOINCONTROL" => vec![(0x200C, 0x200D)],
        "EMOJI" => unicode_prop("Emoji"),
        "EMOJI_PRESENTATION" => unicode_prop("Emoji_Presentation"),
        "EMOJI_MODIFIER" => unicode_prop("Emoji_Modifier"),
        "EMOJI_MODIFIER_BASE" => unicode_prop("Emoji_Modifier_Base"),
        "EMOJI_COMPONENT" => unicode_prop("Emoji_Component"),
        "EXTENDED_PICTOGRAPHIC" => unicode_prop("Extended_Pictographic"),
        _ => return None,
    })
}

/// The code points whose `Character.getType` is one of `cats` (two-letter names).
fn java_categories(cats: &[&str]) -> Ranges {
    const NAMES: [&str; 31] = [
        "Cn", "Lu", "Ll", "Lt", "Lm", "Lo", "Mn", "Me", "Mc", "Nd", "Nl", "No", "Zs", "Zl", "Zp",
        "Cc", "Cf", "", "Co", "Cs", "Pd", "Ps", "Pe", "Pc", "Po", "Sm", "Sc", "Sk", "So", "Pi",
        "Pf",
    ];
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<u32, Ranges>>> =
        OnceLock::new();
    let mask = cats.iter().fold(0u32, |m, c| {
        m | NAMES.iter().position(|n| n == c).map_or(0, |i| 1u32 << i)
    });
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&mask) {
        return r.clone();
    }
    let mut v: Ranges = Vec::new();
    for cp in 0..=0x10FFFFu32 {
        // Surrogate code points are not `char`s but Java gives them type Cs.
        let t = char::from_u32(cp).map_or(19, crate::unicode::get_type);
        if mask & (1 << t) != 0 {
            match v.last_mut() {
                Some(last) if last.1 + 1 == cp => last.1 = cp,
                _ => v.push((cp, cp)),
            }
        }
    }
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(mask, v.clone());
    v
}

/// Unicode blocks (`\p{InGreek}`) by the JDK's names.
fn block(name: &str) -> Option<Ranges> {
    crate::unicode::unicode_block(name).map(<[(u32, u32)]>::to_vec)
}

/// Unicode scripts (`\p{IsLatin}`, `\p{sc=Latn}`) by the JDK's names.
fn script(name: &str) -> Option<Ranges> {
    crate::unicode::unicode_script(name).map(<[(u32, u32)]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(p: &str, flags: u32, text: &str) -> Vec<String> {
        let re = compile(p, flags).unwrap();
        let mut caps = re.captures();
        let mut f = Finder::new();
        let mut out = Vec::new();
        while let Some((s, e)) = f.next(&re, text, &mut caps, true) {
            out.push(text[s..e].to_string());
        }
        out
    }

    /// The same search forced through the backtracker.
    fn all_slow(p: &str, flags: u32, text: &str) -> Vec<String> {
        let mut re = compile(p, flags).unwrap();
        re.fast = None;
        let mut caps = re.captures();
        let mut f = Finder::new();
        let mut out = Vec::new();
        while let Some((s, e)) = f.next(&re, text, &mut caps, true) {
            out.push(text[s..e].to_string());
        }
        out
    }

    fn both(p: &str, flags: u32, text: &str) -> Vec<String> {
        let a = all(p, flags, text);
        assert_eq!(
            a,
            all_slow(p, flags, text),
            "engines disagree on {p:?} / {text:?}"
        );
        a
    }

    fn syntax(p: &str) -> String {
        compile(p, 0).unwrap_err().reason
    }

    #[test]
    fn ascii_classes_by_default() {
        assert_eq!(both("\\w+", 0, "héllo wörld"), vec!["h", "llo", "w", "rld"]);
        assert_eq!(
            both("\\w+", UNICODE_CHARACTER_CLASS, "héllo wörld"),
            vec!["héllo", "wörld"]
        );
        assert_eq!(both("\\W+", 0, "a, b"), vec![", "]);
        assert_eq!(both("[\\w-]+", 0, "a-b c"), vec!["a-b", "c"]);
        assert_eq!(both("\\d+", 0, "12٣4"), vec!["12", "4"]);
        assert_eq!(both("\\p{Alpha}+", 0, "abç"), vec!["ab"]);
        assert_eq!(both("\\p{L}+", 0, "abç1"), vec!["abç"]);
        assert_eq!(both("\\p{IsGreek}+", 0, "aαβ"), vec!["αβ"]);
        assert_eq!(both("\\p{InGreek}+", 0, "aαβ"), vec!["αβ"]);
        assert_eq!(both("[a-z&&[^aeiou]]+", 0, "hello"), vec!["h", "ll"]);
        assert_eq!(both("[^a[b]]+", 0, "abcab"), vec!["c"]);
    }

    #[test]
    fn case_insensitive_and_flags() {
        assert_eq!(both("abc", CASE_INSENSITIVE, "ABC aBc"), vec!["ABC", "aBc"]);
        assert_eq!(both("é", CASE_INSENSITIVE, "É é"), vec!["é"]);
        assert_eq!(
            both("é", CASE_INSENSITIVE | UNICODE_CASE, "É é"),
            vec!["É", "é"]
        );
        assert_eq!(both("[a-c]+", CASE_INSENSITIVE, "AbCd"), vec!["AbC"]);
        assert_eq!(both("(?i)x", 0, "X"), vec!["X"]);
        assert_eq!(both("a(?i:b)c", 0, "aBc aBC"), vec!["aBc"]);
        assert_eq!(both("a b # comment\n c", COMMENTS, "abc"), vec!["abc"]);
        assert_eq!(both("a.b", 0, "a\rb a\nb axb"), vec!["axb"]);
        assert_eq!(both("a.b", DOTALL, "a\rb"), vec!["a\rb"]);
        assert_eq!(both("\\Qa.b\\E", 0, "a.b axb"), vec!["a.b"]);
        assert_eq!(both("a.b", LITERAL, "a.b axb"), vec!["a.b"]);
        assert_eq!(both("\\u0041\\x42\\x{43}\\0104", 0, "ABCD"), vec!["ABCD"]);
        assert_eq!(both("(?<n>a)", 0, "a"), vec!["a"]);
        assert_eq!(both("ab|cd", 0, "xcdab"), vec!["cd", "ab"]);
        assert_eq!(compile("(a)(b(c))", 0).unwrap().groups, 3);
        assert_eq!(
            flags_from_string("case_insensitive|COMMENTS").unwrap(),
            CASE_INSENSITIVE | COMMENTS
        );
        assert_eq!(
            flags_from_string("NOPE").unwrap_err().reason,
            "Unknown regex flag [NOPE]"
        );
    }

    #[test]
    fn empty_matches_follow_java() {
        // After an empty match the search resumes one character later; after a non-empty match
        // an empty match at its end is allowed.
        assert_eq!(both("a*", 0, "baaac"), vec!["", "aaa", "", ""]);
        assert_eq!(both("", 0, "ab"), vec!["", "", ""]);
    }

    #[test]
    fn lookaround_backrefs_atomic() {
        assert_eq!(
            all("(?<=\\D)(?=\\d)|(?<=\\d)(?=\\D)", 0, "ab12cd"),
            vec!["", ""]
        );
        let re = compile("(?<=\\D)(?=\\d)|(?<=\\d)(?=\\D)", 0).unwrap();
        let mut caps = re.captures();
        let mut f = Finder::new();
        let mut at = Vec::new();
        while let Some((s, _)) = f.next(&re, "ab12cd", &mut caps, false) {
            at.push(s);
        }
        assert_eq!(at, vec![2, 4]);
        assert_eq!(all("(\\w)\\1", 0, "abbcdd"), vec!["bb", "dd"]);
        assert_eq!(all("(?<x>a)\\k<x>", 0, "aa"), vec!["aa"]);
        assert_eq!(all("(?i)(a)\\1", 0, "aA"), vec!["aA"]);
        assert_eq!(all("a++a", 0, "aaa"), Vec::<String>::new());
        assert_eq!(all("(?>a+)b", 0, "aab"), vec!["aab"]);
        assert_eq!(all("(?>a|ab)c", 0, "abc"), Vec::<String>::new());
        assert_eq!(all("x(?!y)", 0, "xy xz"), vec!["x"]);
        assert_eq!(all("(?<!a)b", 0, "ab cb"), vec!["b"]);
        assert_eq!(all("(?<=a+)b", 0, "aab"), vec!["b"]);
        assert_eq!(all("\\8", 0, "ab"), Vec::<String>::new());
        assert_eq!(all("(a)|\\1b", 0, "b"), Vec::<String>::new());
        assert_eq!(all("a.*?b", 0, "axbxb"), vec!["axb"]);
        assert_eq!(all("(ab){2,3}?", 0, "abababab"), vec!["abab", "abab"]);
    }

    #[test]
    fn anchors() {
        assert_eq!(both("a$", 0, "a\n"), vec!["a"]);
        assert_eq!(both("a$", 0, "a\r\n"), vec!["a"]);
        assert_eq!(both("a$", 0, "a\n\n"), Vec::<String>::new());
        assert_eq!(both("a\\z", 0, "a\n"), Vec::<String>::new());
        assert_eq!(both("^a", MULTILINE, "a\na\r\na"), vec!["a", "a", "a"]);
        assert_eq!(both("a$", MULTILINE, "a\na\r\na"), vec!["a", "a", "a"]);
        assert_eq!(both("$", MULTILINE, "a\r\n"), vec!["", ""]);
        assert_eq!(both("\\bab\\b", 0, "ab cab ab"), vec!["ab", "ab"]);
        // A non-spacing mark after a letter is part of the word for Java's \b.
        assert_eq!(both("\\b", 0, "e\u{301}x"), vec!["", ""]);
    }

    #[test]
    fn groups_and_replacements() {
        let re = compile("(\\d+)-(?<rest>\\w+)", 0).unwrap();
        let mut caps = re.captures();
        assert!(re.find_at("x 12-ab y", 0, &mut caps));
        assert_eq!(caps.get(1), Some((2, 4)));
        assert_eq!(caps.get(2), Some((5, 7)));
        let mut out = String::new();
        let r = Replacement::parse("<$1|${rest}|\\$>", &re);
        assert!(
            r.replace(&re, "1-a 22-bb", true, &mut caps, &mut out)
                .unwrap()
        );
        assert_eq!(out, "<1|a|$> <22|bb|$>");
        let msg = |rep: &str| Replacement::parse(rep, &re).parts.unwrap_err().reason;
        assert_eq!(msg("$3"), "No group 3");
        assert_eq!(msg("\\"), "character to be escaped is missing");
        assert_eq!(msg("$"), "Illegal group reference: group index is missing");
        assert_eq!(msg("$x"), "Illegal group reference");
        assert_eq!(msg("${nope}"), "No group with name {nope}");
        // `$12` is group 1 then a literal 2 when there is no group 12.
        let mut out = String::new();
        Replacement::parse("$12", &re)
            .replace(&re, "1-a", true, &mut caps, &mut out)
            .unwrap();
        assert_eq!(out, "12");
        assert!(compile("[a-z]+ed", 0).unwrap().full_match("jumped"));
        assert!(!compile("[a-z]+ed", 0).unwrap().full_match("jumpedx"));
        assert!(compile("a|ab", 0).unwrap().full_match("ab"));
    }

    #[test]
    fn syntax_errors_match_java() {
        assert_eq!(
            syntax("[ab"),
            "Unclosed character class near index 2\n[ab\n  ^"
        );
        assert_eq!(syntax("a)"), "Unmatched closing ')' near index 0\na)\n^");
        assert_eq!(
            syntax("*a"),
            "Dangling meta character '*' near index 0\n*a\n^"
        );
        assert_eq!(syntax("a{x}"), "Illegal repetition near index 2\na{x}\n  ^");
        assert_eq!(syntax("a{2"), "Unclosed counted closure near index 3\na{2");
        assert_eq!(syntax("(a"), "Unclosed group near index 2\n(a");
        assert_eq!(
            syntax("\\k<x>"),
            "named capturing group <x> does not exist near index 4\n\\k<x>\n    ^"
        );
        assert_eq!(
            syntax("(?<1a>x)"),
            "capturing group name does not start with a Latin letter near index 3\n(?<1a>x)\n   ^"
        );
        assert_eq!(
            syntax("(?z)"),
            "Unknown inline modifier near index 2\n(?z)\n  ^"
        );
        assert_eq!(
            syntax("\\p{Nope}"),
            "Unknown character property name {Nope} near index 7\n\\p{Nope}\n       ^"
        );
        assert_eq!(
            syntax("a**"),
            "Dangling meta character '*' near index 2\na**\n  ^"
        );
        assert_eq!(
            syntax("x{3,1}"),
            "Illegal repetition range near index 5\nx{3,1}\n     ^"
        );
        assert_eq!(
            syntax("[b-a]"),
            "Illegal character range near index 3\n[b-a]\n   ^"
        );
        assert_eq!(
            syntax("\\"),
            "Unescaped trailing backslash near index 1\n\\"
        );
        assert_eq!(
            syntax("(?<n>a)(?<n>b)"),
            "Named capturing group <n> is already defined near index 11\n(?<n>a)(?<n>b)\n           ^"
        );
        assert_eq!(
            syntax("\\x{110000}"),
            "Hexadecimal codepoint is too big near index 8\n\\x{110000}\n        ^"
        );
        assert_eq!(
            syntax("[]"),
            "Unclosed character class near index 1\n[]\n ^"
        );
        assert_eq!(
            syntax("[^]"),
            "Unclosed character class near index 2\n[^]\n  ^"
        );
        assert_eq!(syntax("(?"), "Unknown inline modifier near index 2\n(?");
        assert_eq!(
            syntax("a{,3}"),
            "Illegal repetition near index 2\na{,3}\n  ^"
        );
        assert_eq!(
            syntax("\\0"),
            "Illegal octal escape sequence near index 2\n\\0"
        );
        assert_eq!(
            syntax("\\08"),
            "Illegal octal escape sequence near index 2\n\\08\n  ^"
        );
        assert_eq!(
            syntax("\\u12"),
            "Illegal Unicode escape sequence near index 4\n\\u12"
        );
        assert_eq!(
            syntax("[a-\\w]"),
            "Illegal character range near index 4\n[a-\\w]\n    ^"
        );
        assert_eq!(
            syntax("(*)"),
            "Dangling meta character '*' near index 1\n(*)\n ^"
        );
        assert_eq!(
            syntax("\\c"),
            "Illegal control escape sequence near index 1\n\\c\n ^"
        );
        assert!(compile("\\Qab", 0).is_ok());
        assert!(compile("\\8", 0).is_ok());
    }
}
