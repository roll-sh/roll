//! Lucene's `RegExp` syntax, compiled to a byte [`Dfa`].
//!
//! The grammar and its quirks follow Lucene 10's recursive-descent parser exactly (operators
//! `|`, `&` (with `INTERSECTION`), concatenation, `? * + {n} {n,} {n,m}`, character classes with
//! `^` negation and `\d \D \s \S \w \W`, `.`, `#` (with `EMPTY`), `@` (with `ANYSTRING`),
//! `"literal"`, `()`, `<n-m>` numeric intervals (with `INTERVAL`) and `<name>` (with `AUTOMATON`,
//! which Elasticsearch never provides)). Error texts and positions (UTF-16 offsets, as in Java)
//! match Lucene's `IllegalArgumentException`s, so `search-es` can wrap them unchanged.
//! `~` complement (with `COMPLEMENT`, part of `ALL`) is supported as Elasticsearch 9 still
//! accepts it: it complements against every valid string (`~a` matches everything but `a`).
//!
//! The AST compiles to a code point NFA (Thompson construction); intersections and complements
//! determinize their operands per code point and embed the product or complement. The result is
//! determinized under the caller's work limit, per code point and then per byte.

use super::cp::CpDfa;
use super::dfa::{Dfa, TooComplex};
use super::nfa::{MAX_CP, Nfa, regexp_case_variants};

/// Syntax flag: `&` intersection.
pub const INTERSECTION: u32 = 0x0001;
/// Syntax flag: `~` complement.
pub const COMPLEMENT: u32 = 0x0002;
/// Syntax flag: `#` empty language.
pub const EMPTY: u32 = 0x0004;
/// Syntax flag: `@` any string.
pub const ANYSTRING: u32 = 0x0008;
/// Syntax flag: `<name>` named automata.
pub const AUTOMATON: u32 = 0x0010;
/// Syntax flag: `<n-m>` numeric intervals.
pub const INTERVAL: u32 = 0x0020;
/// All optional syntax (Elasticsearch's default `flags: ALL`).
pub const ALL: u32 = 0xff;
pub const NONE: u32 = 0;

/// Why a regexp could not be compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegexpError {
    /// Lucene's `IllegalArgumentException` text.
    Syntax(String),
    /// Lucene's `TooComplexToDeterminizeException`.
    TooComplex(TooComplex),
}

impl std::fmt::Display for RegexpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegexpError::Syntax(s) => f.write_str(s),
            RegexpError::TooComplex(t) => t.fmt(f),
        }
    }
}

impl std::error::Error for RegexpError {}

impl From<TooComplex> for RegexpError {
    fn from(t: TooComplex) -> RegexpError {
        RegexpError::TooComplex(t)
    }
}

/// Parsed regular expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Union(Box<Node>, Box<Node>),
    Concat(Box<Node>, Box<Node>),
    Intersection(Box<Node>, Box<Node>),
    Optional(Box<Node>),
    /// `min` or more (`max: None`) or `min..=max` repetitions.
    Repeat(Box<Node>, u32, Option<u32>),
    Char(u32),
    /// Inclusive code point ranges.
    Class(Vec<(u32, u32)>),
    AnyChar,
    Empty,
    AnyString,
    Str(String),
    Interval {
        min: u64,
        max: u64,
        digits: usize,
    },
    Named(String),
    Complement(Box<Node>),
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    flags: u32,
    case_insensitive: bool,
    _src: &'a str,
}

type PResult<T> = Result<T, RegexpError>;

fn err<T>(msg: impl Into<String>) -> PResult<T> {
    Err(RegexpError::Syntax(msg.into()))
}

impl Parser<'_> {
    /// Position in UTF-16 code units, as Java reports it.
    fn jpos(&self) -> usize {
        self.chars[..self.pos.min(self.chars.len())]
            .iter()
            .map(|c| c.len_utf16())
            .sum()
    }
    fn more(&self) -> bool {
        self.pos < self.chars.len()
    }
    fn peek(&self, set: &str) -> bool {
        self.more() && set.contains(self.chars[self.pos])
    }
    fn matches(&mut self, c: char) -> bool {
        if self.more() && self.chars[self.pos] == c {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn next(&mut self) -> PResult<char> {
        if !self.more() {
            return err("unexpected end-of-string");
        }
        self.pos += 1;
        Ok(self.chars[self.pos - 1])
    }
    fn check(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }

    fn union(&mut self) -> PResult<Node> {
        let mut r = self.inter()?;
        while self.matches('|') {
            let e = self.inter()?;
            r = Node::Union(Box::new(r), Box::new(e));
        }
        Ok(r)
    }

    fn inter(&mut self) -> PResult<Node> {
        let mut r = self.concat()?;
        while self.check(INTERSECTION) && self.matches('&') {
            let e = self.concat()?;
            r = Node::Intersection(Box::new(r), Box::new(e));
        }
        Ok(r)
    }

    fn concat(&mut self) -> PResult<Node> {
        let mut r = self.repeat()?;
        while self.more() && !self.peek(")|") && (!self.check(INTERSECTION) || !self.peek("&")) {
            let e = self.repeat()?;
            r = Node::Concat(Box::new(r), Box::new(e));
        }
        Ok(r)
    }

    fn integer(&mut self) -> PResult<Option<u32>> {
        let start = self.pos;
        while self.peek("0123456789") {
            self.pos += 1;
        }
        if start == self.pos {
            return Ok(None);
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        // Java's Integer.parseInt overflows into a NumberFormatException.
        s.parse::<u32>()
            .ok()
            .filter(|&v| v <= i32::MAX as u32)
            .map(Some)
            .ok_or_else(|| RegexpError::Syntax(format!("For input string: \"{s}\"")))
    }

    fn compl(&mut self) -> PResult<Node> {
        if self.check(COMPLEMENT) && self.matches('~') {
            return Ok(Node::Complement(Box::new(self.compl()?)));
        }
        self.char_class_exp()
    }

    fn repeat(&mut self) -> PResult<Node> {
        let mut e = self.compl()?;
        while self.peek("?*+{") {
            if self.matches('?') {
                e = Node::Optional(Box::new(e));
            } else if self.matches('*') {
                e = Node::Repeat(Box::new(e), 0, None);
            } else if self.matches('+') {
                e = Node::Repeat(Box::new(e), 1, None);
            } else if self.matches('{') {
                let Some(n) = self.integer()? else {
                    return err(format!("integer expected at position {}", self.jpos()));
                };
                let m = if self.matches(',') {
                    self.integer()?
                } else {
                    Some(n)
                };
                if !self.matches('}') {
                    return err(format!("expected '}}' at position {}", self.jpos()));
                }
                if let Some(m) = m
                    && n > m
                {
                    return err(format!("invalid repetition range(out of order): {n}..{m}"));
                }
                e = Node::Repeat(Box::new(e), n, m);
            }
        }
        Ok(e)
    }

    fn char_class_exp(&mut self) -> PResult<Node> {
        if self.matches('[') {
            let negate = self.matches('^');
            let ranges = self.char_classes()?;
            if !self.matches(']') {
                return err(format!("expected ']' at position {}", self.jpos()));
            }
            if negate {
                // A class of one character becomes Lucene's single-char node, which case
                // folding applies to (even when it was escaped or written as a range `a-a`).
                let ranges = match ranges.as_slice() {
                    [(a, b)] if a == b && self.case_insensitive => {
                        regexp_case_variants(*a).into_iter().map(|v| (v, v)).collect()
                    }
                    _ => ranges,
                };
                return Ok(Node::Class(complement(&ranges)));
            }
            Ok(if ranges.len() == 1 && ranges[0].0 == ranges[0].1 {
                Node::Char(ranges[0].0)
            } else {
                Node::Class(ranges)
            })
        } else {
            self.simple()
        }
    }

    fn char_classes(&mut self) -> PResult<Vec<(u32, u32)>> {
        let mut ranges = Vec::new();
        loop {
            if self.matches('\\') {
                if self.peek("\\ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz") {
                    self.predefined(&mut ranges)?;
                } else {
                    let c = self.next()? as u32;
                    ranges.push((c, c));
                }
            } else {
                let c = self.char_exp()? as u32;
                if self.matches('-') {
                    let d = self.char_exp()? as u32;
                    if c > d {
                        return err(format!("invalid range: from ({c}) cannot be > to ({d})"));
                    }
                    ranges.push((c, d));
                } else if self.case_insensitive {
                    for v in regexp_case_variants(c) {
                        ranges.push((v, v));
                    }
                } else {
                    ranges.push((c, c));
                }
            }
            if !(self.more() && !self.peek("]")) {
                break;
            }
        }
        Ok(ranges)
    }

    fn predefined(&mut self, r: &mut Vec<(u32, u32)>) -> PResult<()> {
        let max = MAX_CP;
        let c = self.chars[self.pos];
        let push = |r: &mut Vec<(u32, u32)>, v: &[(u32, u32)]| r.extend_from_slice(v);
        match c {
            '\\' => push(r, &[('\\' as u32, '\\' as u32)]),
            'd' => push(r, &[('0' as u32, '9' as u32)]),
            'D' => push(r, &[(0, '0' as u32 - 1), ('9' as u32 + 1, max)]),
            's' => push(
                r,
                &[
                    ('\t' as u32, '\n' as u32),
                    ('\r' as u32, '\r' as u32),
                    (' ' as u32, ' ' as u32),
                ],
            ),
            'S' => push(
                r,
                &[
                    (0, '\t' as u32 - 1),
                    ('\n' as u32 + 1, '\r' as u32 - 1),
                    ('\r' as u32 + 1, ' ' as u32 - 1),
                    (' ' as u32 + 1, max),
                ],
            ),
            'w' => push(
                r,
                &[
                    ('0' as u32, '9' as u32),
                    ('A' as u32, 'Z' as u32),
                    ('_' as u32, '_' as u32),
                    ('a' as u32, 'z' as u32),
                ],
            ),
            'W' => push(
                r,
                &[
                    (0, '0' as u32 - 1),
                    ('9' as u32 + 1, 'A' as u32 - 1),
                    ('Z' as u32 + 1, '_' as u32 - 1),
                    ('_' as u32 + 1, 'a' as u32 - 1),
                    ('z' as u32 + 1, max),
                ],
            ),
            _ => {
                self.pos += 1;
                // Java appends the code point as a decimal int.
                return err(format!("invalid character class \\{}", c as u32));
            }
        }
        self.pos += 1;
        Ok(())
    }

    fn simple(&mut self) -> PResult<Node> {
        if self.matches('.') {
            return Ok(Node::AnyChar);
        }
        if self.check(EMPTY) && self.matches('#') {
            return Ok(Node::Empty);
        }
        if self.check(ANYSTRING) && self.matches('@') {
            return Ok(Node::AnyString);
        }
        if self.matches('"') {
            let start = self.pos;
            while self.more() && !self.peek("\"") {
                self.pos += 1;
            }
            if !self.matches('"') {
                return err(format!("expected '\"' at position {}", self.jpos()));
            }
            return Ok(Node::Str(self.chars[start..self.pos - 1].iter().collect()));
        }
        if self.matches('(') {
            if self.matches(')') {
                return Ok(Node::Str(String::new()));
            }
            let e = self.union()?;
            if !self.matches(')') {
                return err(format!("expected ')' at position {}", self.jpos()));
            }
            return Ok(e);
        }
        if (self.check(AUTOMATON) || self.check(INTERVAL)) && self.matches('<') {
            let start = self.pos;
            while self.more() && !self.peek(">") {
                self.pos += 1;
            }
            if !self.matches('>') {
                return err(format!("expected '>' at position {}", self.jpos()));
            }
            let s: String = self.chars[start..self.pos - 1].iter().collect();
            let at = self.jpos() - 1;
            return match s.find('-') {
                None => {
                    if !self.check(AUTOMATON) {
                        return err(format!("interval syntax error at position {at}"));
                    }
                    Ok(Node::Named(s))
                }
                Some(i) => {
                    if !self.check(INTERVAL) {
                        return err(format!("illegal identifier at position {at}"));
                    }
                    let bad =
                        || RegexpError::Syntax(format!("interval syntax error at position {at}"));
                    if i == 0 || i == s.len() - 1 || Some(i) != s.rfind('-') {
                        return Err(bad());
                    }
                    let (smin, smax) = (&s[..i], &s[i + 1..]);
                    let parse = |x: &str| -> Option<u64> {
                        // Java's Integer.parseInt: optional sign, ASCII digits, fits in int.
                        let v: i64 = x.parse().ok()?;
                        (0..=i32::MAX as i64).contains(&v).then_some(v as u64)
                    };
                    let (Some(mut a), Some(mut b)) = (parse(smin), parse(smax)) else {
                        return Err(bad());
                    };
                    let digits = if smin.len() == smax.len() {
                        smin.len()
                    } else {
                        0
                    };
                    if a > b {
                        std::mem::swap(&mut a, &mut b);
                    }
                    Ok(Node::Interval {
                        min: a,
                        max: b,
                        digits,
                    })
                }
            };
        }
        // Predefined class outside brackets; note Lucene consumes the backslash either way.
        if self.matches('\\') && self.peek("\\ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz")
        {
            let mut r = Vec::new();
            self.predefined(&mut r)?;
            return Ok(Node::Class(r));
        }
        let c = self.char_exp()?;
        Ok(Node::Char(c as u32))
    }

    fn char_exp(&mut self) -> PResult<char> {
        self.matches('\\');
        self.next()
    }
}

fn complement(ranges: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut v = ranges.to_vec();
    v.sort_unstable();
    let mut out = Vec::new();
    let mut next = 0u32;
    for (a, b) in v {
        if a > next {
            out.push((next, a - 1));
        }
        next = next.max(b.saturating_add(1));
    }
    if next <= MAX_CP {
        out.push((next, MAX_CP));
    }
    out
}

/// Collapses runs of repetition operators (`+`, `*`, `?`) outside escapes, character classes and
/// quoted strings into one equivalent operator (equal ones stay, mixed ones become `*`), as
/// Elasticsearch does to every `regexp` query on string fields before Lucene parses it (stacked
/// quantifiers only add states). Error positions refer to the collapsed pattern, so call this
/// before [`compile`] when Elasticsearch's messages must match.
pub fn collapse_consecutive_quantifiers(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    let (mut in_class, mut in_quote, mut prev_quantifier) = (false, false, false);
    while let Some(c) = chars.next() {
        let quantifier = matches!(c, '+' | '*' | '?');
        if c == '\\' && chars.peek().is_some() {
            out.push(c);
            out.push(chars.next().unwrap_or('\\'));
        } else if in_quote {
            out.push(c);
            in_quote = c != '"';
        } else if in_class {
            out.push(c);
            in_class = c != ']';
        } else if c == '"' {
            out.push(c);
            in_quote = true;
        } else if c == '[' {
            out.push(c);
            in_class = true;
        } else if quantifier && prev_quantifier {
            let last = out.pop().unwrap_or(c);
            out.push(if last == c { c } else { '*' });
            continue;
        } else {
            out.push(c);
            prev_quantifier = quantifier;
            continue;
        }
        prev_quantifier = false;
    }
    out
}

/// Parses `pattern` with Lucene's syntax flags; `case_insensitive` is the `CASE_INSENSITIVE`
/// match flag (Elasticsearch's `case_insensitive: true`).
pub fn parse(pattern: &str, flags: u32, case_insensitive: bool) -> Result<Node, RegexpError> {
    if flags > ALL {
        return err("Illegal syntax flag");
    }
    if pattern.is_empty() {
        return Ok(Node::Str(String::new()));
    }
    let mut p = Parser {
        chars: pattern.chars().collect(),
        pos: 0,
        flags,
        case_insensitive,
        _src: pattern,
    };
    let e = p.union()?;
    if p.more() {
        return err(format!("end-of-string expected at position {}", p.jpos()));
    }
    Ok(e)
}

/// Parses and determinizes `pattern` under `work_limit` (Elasticsearch's
/// `max_determinized_states`, default [`super::dfa::DEFAULT_WORK_LIMIT`]).
pub fn compile(
    pattern: &str,
    flags: u32,
    case_insensitive: bool,
    work_limit: u32,
) -> Result<Dfa, RegexpError> {
    let node = parse(pattern, flags, case_insensitive)?;
    let mut nfa = Nfa::new();
    let (s, e) = build(&node, &mut nfa, case_insensitive, work_limit)?;
    nfa.set_start(s);
    nfa.set_accept(e, true);
    Ok(Dfa::from_nfa(&nfa, work_limit)?)
}

fn build(n: &Node, nfa: &mut Nfa, ci: bool, limit: u32) -> Result<(u32, u32), RegexpError> {
    let s = nfa.add_state();
    let e = nfa.add_state();
    match n {
        Node::Char(c) => {
            if ci {
                for v in regexp_case_variants(*c) {
                    nfa.add_char(s, v, e);
                }
            } else {
                nfa.add_char(s, *c, e);
            }
        }
        Node::Class(ranges) => {
            for &(a, b) in ranges {
                nfa.add_range(s, a, b, e);
            }
        }
        Node::AnyChar => nfa.add_any(s, e),
        Node::Empty => {}
        Node::AnyString => {
            nfa.add_any(s, s);
            nfa.add_epsilon(s, e);
        }
        Node::Str(text) => {
            let mut cur = s;
            for c in text.chars() {
                let n2 = nfa.add_state();
                if ci {
                    for v in regexp_case_variants(c as u32) {
                        nfa.add_char(cur, v, n2);
                    }
                } else {
                    nfa.add_char(cur, c as u32, n2);
                }
                cur = n2;
            }
            nfa.add_epsilon(cur, e);
        }
        Node::Concat(a, b) => {
            let (a1, a2) = build(a, nfa, ci, limit)?;
            let (b1, b2) = build(b, nfa, ci, limit)?;
            nfa.add_epsilon(s, a1);
            nfa.add_epsilon(a2, b1);
            nfa.add_epsilon(b2, e);
        }
        Node::Union(a, b) => {
            for x in [a, b] {
                let (x1, x2) = build(x, nfa, ci, limit)?;
                nfa.add_epsilon(s, x1);
                nfa.add_epsilon(x2, e);
            }
        }
        Node::Optional(a) => {
            let (a1, a2) = build(a, nfa, ci, limit)?;
            nfa.add_epsilon(s, a1);
            nfa.add_epsilon(a2, e);
            nfa.add_epsilon(s, e);
        }
        Node::Repeat(a, min, max) => {
            let mut cur = s;
            for _ in 0..*min {
                let (a1, a2) = build(a, nfa, ci, limit)?;
                nfa.add_epsilon(cur, a1);
                cur = a2;
            }
            match max {
                None => {
                    let (a1, a2) = build(a, nfa, ci, limit)?;
                    // Lucene's `Operations.repeat` returns an empty-language operand unchanged:
                    // `#*` matches nothing, not the empty string.
                    if *min == 0 && !reachable(nfa, a1, a2) {
                        return Ok((s, e));
                    }
                    let hub = nfa.add_state();
                    nfa.add_epsilon(cur, hub);
                    nfa.add_epsilon(hub, a1);
                    nfa.add_epsilon(a2, hub);
                    nfa.add_epsilon(hub, e);
                }
                Some(m) => {
                    nfa.add_epsilon(cur, e);
                    for _ in *min..*m {
                        let (a1, a2) = build(a, nfa, ci, limit)?;
                        nfa.add_epsilon(cur, a1);
                        nfa.add_epsilon(a2, e);
                        cur = a2;
                    }
                }
            }
        }
        Node::Intersection(a, b) => {
            let p = sub_dfa(a, ci, limit)?.intersection(&sub_dfa(b, ci, limit)?);
            if let Some(start) = p.embed(nfa, e) {
                nfa.add_epsilon(s, start);
            }
        }
        Node::Interval { min, max, digits } => interval(nfa, s, e, *min, *max, *digits),
        Node::Named(name) => return err(format!("'{name}' not found")),
        Node::Complement(a) => {
            // Lucene bounds `~` (DEPRECATED_COMPLEMENT) by the default limit, whatever the
            // query's max_determinized_states.
            let d = sub_dfa(a, ci, super::dfa::DEFAULT_WORK_LIMIT)?.complement();
            if let Some(start) = d.embed(nfa, e) {
                nfa.add_epsilon(s, start);
            }
        }
    }
    Ok((s, e))
}

/// Whether `to` can be reached from `from` (a fragment's language is empty when its end
/// cannot be reached from its start).
fn reachable(nfa: &Nfa, from: u32, to: u32) -> bool {
    let mut seen = vec![false; nfa.states.len()];
    let mut stack = vec![from];
    while let Some(x) = stack.pop() {
        if x == to {
            return true;
        }
        if std::mem::replace(&mut seen[x as usize], true) {
            continue;
        }
        let st = &nfa.states[x as usize];
        stack.extend(st.eps.iter().copied());
        stack.extend(st.ranges.iter().map(|r| r.2));
        stack.extend(st.bytes.iter().map(|r| r.2));
    }
    false
}

/// A sub-expression determinized per code point (operands of `&` and `~`).
fn sub_dfa(n: &Node, ci: bool, limit: u32) -> Result<CpDfa, RegexpError> {
    let mut nfa = Nfa::new();
    let (s, e) = build(n, &mut nfa, ci, limit)?;
    nfa.set_start(s);
    nfa.set_accept(e, true);
    Ok(CpDfa::determinize(&nfa, limit)?)
}

/// Decimal strings whose value is in `[min, max]`: exactly `digits` long (zero padded) when
/// `digits > 0`, otherwise with any number of leading zeros.
fn interval(nfa: &mut Nfa, s: u32, e: u32, min: u64, max: u64, digits: usize) {
    if digits > 0 {
        let lo = format!("{min:0digits$}");
        let hi = format!("{max:0digits$}");
        if lo.len() == digits && hi.len() == digits {
            fixed_range(nfa, s, e, lo.as_bytes(), hi.as_bytes());
        }
        return;
    }
    let zeros = nfa.add_state();
    nfa.add_epsilon(s, zeros);
    nfa.add_char(zeros, '0' as u32, zeros);
    let width = max.to_string().len();
    for len in 1..=width {
        let lo_len = if len == 1 {
            0
        } else {
            10u64.pow(len as u32 - 1)
        };
        let hi_len = 10u64.pow(len as u32) - 1;
        let (a, b) = (min.max(lo_len), max.min(hi_len));
        if a > b {
            continue;
        }
        let lo = format!("{a:0len$}");
        let hi = format!("{b:0len$}");
        fixed_range(nfa, zeros, e, lo.as_bytes(), hi.as_bytes());
    }
}

/// Equal-length digit strings between `lo` and `hi` inclusive.
fn fixed_range(nfa: &mut Nfa, s: u32, e: u32, lo: &[u8], hi: &[u8]) {
    if lo.is_empty() {
        nfa.add_epsilon(s, e);
        return;
    }
    let (l, h) = (lo[0] as u32, hi[0] as u32);
    let rest = lo.len() - 1;
    if l == h {
        let m = nfa.add_state();
        nfa.add_char(s, l, m);
        fixed_range(nfa, m, e, &lo[1..], &hi[1..]);
        return;
    }
    let nines = vec![b'9'; rest];
    let zeros = vec![b'0'; rest];
    let m = nfa.add_state();
    nfa.add_char(s, l, m);
    fixed_range(nfa, m, e, &lo[1..], &nines);
    if l + 1 < h {
        let m = nfa.add_state();
        nfa.add_range(s, l + 1, h - 1, m);
        fixed_range(nfa, m, e, &zeros, &nines);
    }
    let m = nfa.add_state();
    nfa.add_char(s, h, m);
    fixed_range(nfa, m, e, &zeros, &hi[1..]);
}

#[cfg(test)]
mod tests {
    use super::super::Automaton;
    use super::super::dfa::DEFAULT_WORK_LIMIT;
    use super::*;

    fn re(p: &str) -> Dfa {
        compile(p, ALL, false, DEFAULT_WORK_LIMIT).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    fn syntax_err(p: &str, flags: u32) -> String {
        match compile(p, flags, false, DEFAULT_WORK_LIMIT) {
            Err(RegexpError::Syntax(s)) => s,
            other => panic!("expected syntax error for {p:?}, got {other:?}"),
        }
    }

    #[test]
    fn basics() {
        let d = re("ab+c?");
        assert!(d.matches(b"ab"));
        assert!(d.matches(b"abbbc"));
        assert!(!d.matches(b"ac"));
        let d = re("a.c");
        assert!(d.matches("a日c".as_bytes()));
        assert!(!d.matches(b"ac"));
        let d = re("(foo|bar)baz");
        assert!(d.matches(b"foobaz") && d.matches(b"barbaz") && !d.matches(b"baz"));
        let d = re("a{2,3}");
        assert!(!d.matches(b"a") && d.matches(b"aa") && d.matches(b"aaa") && !d.matches(b"aaaa"));
        let d = re("a{2,}");
        assert!(!d.matches(b"a") && d.matches(b"aaaaa"));
        let d = re("a{0}");
        assert!(d.matches(b"") && !d.matches(b"a"));
        let d = re("[a-c]x[^a-c]");
        assert!(d.matches(b"bxd") && !d.matches(b"bxa") && d.matches("bx日".as_bytes()));
        let d = re(r"\d+\.\w");
        assert!(d.matches(b"12.a") && !d.matches(b"12xa"));
        let d = re("\"a.b\"");
        assert!(d.matches(b"a.b") && !d.matches(b"axb"));
        let d = re("@");
        assert!(d.matches(b"") && d.matches(b"anything"));
        assert!(re("#").is_empty_language());
        let d = re("@&.*b.*&.{2}");
        assert!(d.matches(b"ab") && d.matches(b"bb") && !d.matches(b"abc") && !d.matches(b"aa"));
        assert!(re("").matches(b""));
        assert!(re("()").matches(b""));
        // Like Lucene, an empty alternative is a syntax error.
        assert_eq!(syntax_err("a|", ALL), "unexpected end-of-string");
    }

    #[test]
    fn intervals() {
        let d = re("<5-100>");
        for (s, want) in [
            ("5", true),
            ("05", true),
            ("100", true),
            ("0100", true),
            ("4", false),
            ("101", false),
            ("50", true),
            ("", false),
        ] {
            assert_eq!(d.matches(s.as_bytes()), want, "{s}");
        }
        let d = re("<010-120>");
        for (s, want) in [
            ("010", true),
            ("10", false),
            ("099", true),
            ("120", true),
            ("121", false),
            ("0010", false),
        ] {
            assert_eq!(d.matches(s.as_bytes()), want, "{s}");
        }
        let d = re("x<9-1>");
        assert!(d.matches(b"x5"));
    }

    #[test]
    fn collapses_stacked_quantifiers_like_elasticsearch() {
        for (a, b) in [
            ("a+*", "a*"),
            ("a++", "a+"),
            ("a??", "a?"),
            ("a+?+", "a*"),
            ("a*b?+c", "a*b*c"),
            ("\\+*", "\\+*"),
            ("\\**", "\\**"),
            ("[+*?]+*", "[+*?]*"),
            ("\"a++\"?*", "\"a++\"*"),
            ("(ab)+*?", "(ab)*"),
            ("a\\", "a\\"),
            ("", ""),
            ("日+?", "日*"),
        ] {
            assert_eq!(collapse_consecutive_quantifiers(a), b, "{a}");
        }
    }

    #[test]
    fn case_insensitive() {
        let d = compile("Ab[c]\"dE\"", ALL, true, DEFAULT_WORK_LIMIT).unwrap();
        assert!(d.matches(b"aBCde") && d.matches(b"ABcDE"));
    }

    #[test]
    fn flags_disable_operators() {
        let d = compile("a&b", NONE, false, DEFAULT_WORK_LIMIT);
        // Without INTERSECTION, '&' is an ordinary character.
        assert!(d.unwrap().matches(b"a&b"));
        assert!(
            compile("@", NONE, false, DEFAULT_WORK_LIMIT)
                .unwrap()
                .matches(b"@")
        );
    }

    #[test]
    fn errors_match_lucene() {
        assert_eq!(syntax_err("(a", ALL), "expected ')' at position 2");
        assert_eq!(
            syntax_err("a)", ALL),
            "end-of-string expected at position 1"
        );
        assert_eq!(syntax_err("[ab", ALL), "expected ']' at position 3");
        assert_eq!(syntax_err("a{", ALL), "integer expected at position 2");
        assert_eq!(syntax_err("a{2", ALL), "expected '}' at position 3");
        assert_eq!(
            syntax_err("a{3,2}", ALL),
            "invalid repetition range(out of order): 3..2"
        );
        assert_eq!(
            syntax_err("[z-a]", ALL),
            "invalid range: from (122) cannot be > to (97)"
        );
        assert_eq!(syntax_err(r"\p", ALL), "invalid character class \\112");
        assert_eq!(syntax_err("\\", ALL), "unexpected end-of-string");
        assert_eq!(syntax_err("<foo>", ALL), "'foo' not found");
        assert_eq!(
            syntax_err("<1-x>", ALL),
            "interval syntax error at position 4"
        );
        assert_eq!(syntax_err("\"ab", ALL), "expected '\"' at position 3");
        assert_eq!(syntax_err("日(", ALL), "unexpected end-of-string");
    }

    #[test]
    fn too_complex() {
        let e = compile("[ac]*a[ac]{50,200}", ALL, false, DEFAULT_WORK_LIMIT).unwrap_err();
        assert!(matches!(e, RegexpError::TooComplex(_)), "{e:?}");
    }
}
