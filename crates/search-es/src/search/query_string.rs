//! `query_string` (Lucene's classic query parser with Elasticsearch's `QueryStringQueryParser`)
//! and `simple_query_string` (Lucene's `SimpleQueryParser` with `SimpleQueryStringQueryParser`).
//!
//! The classic syntax: clauses with `+`/`-`/`NOT` modifiers, `AND`/`OR`/`&&`/`||`
//! conjunctions combined by Lucene's `addClause` rules, `field:` prefixes (patterns and
//! `_exists_`), groups, `"phrases"~slop`, `term~fuzzy`, `prefix*` / `wild?card`, `/regexp/`,
//! `[a TO b]` / `{a TO b}` / `>=x` ranges and `^boost`. Unfielded terms go to the default fields
//! through the multi-match machinery.

use super::query::{QueryCtx, arc, match_none};
use super::text::{self, MatchOpts, MatchType};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::query::full_text::{MultiMatchType, Operator, QueryString, SimpleQueryString, sqs_flags};
use crate::query::term_level::Fuzziness;
use search_index::query::{BooleanQuery, BoostQuery, DisMaxQuery, Occur};
use search_index::QueryRef;

/// `field^boost`.
pub fn parse_field_boost(s: &str) -> (String, f32) {
    match s.rsplit_once('^') {
        Some((f, b)) => (f.to_string(), b.parse().unwrap_or(1.0)),
        None => (s.to_string(), 1.0),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tk {
    Term(String, bool /* had wildcard chars unescaped */),
    Phrase(String),
    Regex(String),
    Range { lo: String, hi: String, il: bool, iu: bool },
    Field(String),
    LParen,
    RParen,
    Plus,
    Minus,
    Not,
    And,
    Or,
    Caret(String),
    Tilde(Option<String>),
}

/// `QueryParserBase.parse`: Lucene's `ParseException` (or `TokenMgrError`, for lexical
/// errors) wrapped in a `ParseException` naming the query. Neither is an Elasticsearch
/// exception, so the root cause stays the `query_shard_exception` around them.
fn parse_error(q: &str, msg: &str) -> EsError {
    let inner = if msg.starts_with("Lexical error") { "token_mgr_error" } else { "parse_exception" };
    EsError::java(400, "parse_exception", format!("Cannot parse '{q}': {msg}")).caused_by(EsError::java(400, inner, msg))
}

// Lucene's `QueryParserConstants` token kinds, the ones syntax errors name.
const K_EOF: u8 = 0;
const K_AND: u8 = 8;
const K_OR: u8 = 9;
const K_NOT: u8 = 10;
const K_PLUS: u8 = 11;
const K_MINUS: u8 = 12;
const K_BAREOPER: u8 = 13;
const K_LPAREN: u8 = 14;
const K_RPAREN: u8 = 15;
const K_COLON: u8 = 16;
const K_STAR: u8 = 17;
const K_CARAT: u8 = 18;
const K_QUOTED: u8 = 19;
const K_TERM: u8 = 20;
const K_FUZZY_SLOP: u8 = 21;
const K_PREFIXTERM: u8 = 22;
const K_WILDTERM: u8 = 23;
const K_REGEXPTERM: u8 = 24;
const K_RANGEIN_START: u8 = 25;
const K_RANGEEX_START: u8 = 26;
const K_NUMBER: u8 = 27;
const K_RANGE_TO: u8 = 28;
const K_RANGEIN_END: u8 = 29;
const K_RANGEEX_END: u8 = 30;
const K_RANGE_QUOTED: u8 = 31;
const K_RANGE_GOOP: u8 = 32;

/// `QueryParserConstants.tokenImage`.
fn token_image(kind: u8) -> &'static str {
    match kind {
        K_EOF => "<EOF>",
        K_AND => "<AND>",
        K_OR => "<OR>",
        K_NOT => "<NOT>",
        K_PLUS => "\"+\"",
        K_MINUS => "\"-\"",
        K_BAREOPER => "<BAREOPER>",
        K_LPAREN => "\"(\"",
        K_RPAREN => "\")\"",
        K_COLON => "\":\"",
        K_STAR => "\"*\"",
        K_CARAT => "\"^\"",
        K_QUOTED => "<QUOTED>",
        K_TERM => "<TERM>",
        K_FUZZY_SLOP => "<FUZZY_SLOP>",
        K_PREFIXTERM => "<PREFIXTERM>",
        K_WILDTERM => "<WILDTERM>",
        K_REGEXPTERM => "<REGEXPTERM>",
        K_RANGEIN_START => "\"[\"",
        K_RANGEEX_START => "\"{\"",
        K_NUMBER => "<NUMBER>",
        K_RANGE_TO => "\"TO\"",
        K_RANGEIN_END => "\"]\"",
        K_RANGEEX_END => "\"}\"",
        K_RANGE_QUOTED => "<RANGE_QUOTED>",
        _ => "<RANGE_GOOP>",
    }
}

/// The tokens a clause can start with once its field prefix is read (`Clause` → `Term` or
/// a group).
const CLAUSE_BODY: &[u8] = &[K_BAREOPER, K_LPAREN, K_STAR, K_QUOTED, K_TERM, K_PREFIXTERM, K_WILDTERM, K_REGEXPTERM, K_RANGEIN_START, K_RANGEEX_START, K_NUMBER];

/// JavaCC's `ParseException.initialise` message for an unexpected token: the token (kind and
/// image) or `<EOF>`, its column (`FastCharStream` reports the offset in the query, always on
/// line 1) and the expected token kinds, one sequence per line.
fn syntax_message(found: Option<(u8, &str)>, column: usize, expected: &[u8]) -> String {
    let mut m = String::from("Encountered \"");
    match found {
        None => m.push_str("<EOF>"),
        Some((kind, image)) => {
            m.push(' ');
            m.push_str(token_image(kind));
            m.push_str(" \"");
            m.push_str(&java_cc_escapes(image));
            m.push_str(" \"");
        }
    }
    m.push_str(&format!("\" at line 1, column {column}.\n"));
    m.push_str(if expected.len() == 1 { "Was expecting:\n    " } else { "Was expecting one of:\n    " });
    for &k in expected {
        m.push_str(token_image(k));
        m.push(' ');
        if k != K_EOF {
            m.push_str("...");
        }
        m.push_str("\n    ");
    }
    m
}

/// JavaCC's `TokenMgrError.LexicalErr` for the char at `i` that can't start a boost `NUMBER`
/// (the `Boost` state, lexical state 0, which the message doesn't name). When it is the last
/// UTF-16 unit, JavaCC's look past it hits the end: the column moves on and names `<EOF>`.
fn boost_lexical_error(chars: &[char], u16_at: &[usize], i: usize) -> String {
    let unit = chars[i].encode_utf16(&mut [0u16; 2])[0];
    if u16_at[i] + 1 == u16_at[chars.len()] {
        format!("Lexical error at line 1, column {}.  Encountered: <EOF>", u16_at[i] + 2)
    } else {
        format!("Lexical error at line 1, column {}.  Encountered: '{unit}' ({unit}),", u16_at[i] + 1)
    }
}

/// JavaCC's `add_escapes`.
fn java_cc_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for u in s.encode_utf16() {
        match u {
            0 => {}
            8 => out.push_str("\\b"),
            9 => out.push_str("\\t"),
            10 => out.push_str("\\n"),
            12 => out.push_str("\\f"),
            13 => out.push_str("\\r"),
            0x22 => out.push_str("\\\""),
            0x27 => out.push_str("\\'"),
            0x5c => out.push_str("\\\\"),
            u if !(0x20..=0x7e).contains(&u) => out.push_str(&format!("\\u{u:04x}")),
            u => out.push(u as u8 as char),
        }
    }
    out
}

/// Lucene's `QueryParserTokenManager`, roughly: the classic syntax as tokens.
#[cfg(test)]
fn tokenize(q: &str) -> EsResult<Vec<Tk>> {
    tokenize_spanned(q).map(|(t, _)| t)
}

/// Each token's `(start, end)` offsets in UTF-16 units of the query (Lucene's columns).
type Spans = Vec<(usize, usize)>;

/// [`tokenize`], with the tokens' [`Spans`].
fn tokenize_spanned(q: &str) -> EsResult<(Vec<Tk>, Spans)> {
    let chars: Vec<char> = q.chars().collect();
    let mut u16_at = Vec::with_capacity(chars.len() + 1);
    let mut off = 0;
    for c in &chars {
        u16_at.push(off);
        off += c.len_utf16();
    }
    u16_at.push(off);
    let mut i = 0;
    let mut out = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut token_start = 0;
    let special = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{3000}' | '+' | '-' | '!' | '(' | ')' | ':' | '^' | '[' | ']' | '"' | '{' | '}' | '~' | '\\' | '/');
    while i < chars.len() {
        // The tokens the last step pushed span from where it started to where it stopped.
        spans.resize(out.len(), (u16_at[token_start], u16_at[i]));
        token_start = i;
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' | '\u{3000}' => i += 1,
            '(' => {
                out.push(Tk::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tk::RParen);
                i += 1;
            }
            '+' => {
                out.push(Tk::Plus);
                i += 1;
            }
            '-' => {
                out.push(Tk::Minus);
                i += 1;
            }
            '!' => {
                out.push(Tk::Not);
                i += 1;
            }
            '&' if chars.get(i + 1) == Some(&'&') => {
                out.push(Tk::And);
                i += 2;
            }
            '|' if chars.get(i + 1) == Some(&'|') => {
                out.push(Tk::Or);
                i += 2;
            }
            '^' => {
                // `^` switches to the `Boost` lexical state, where only a `NUMBER`
                // (`digits ["." digits]`) may follow.
                i += 1;
                let s = i;
                if i < chars.len() && !chars[i].is_ascii_digit() {
                    return Err(parse_error(q, &boost_lexical_error(&chars, &u16_at, i)));
                }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                out.push(Tk::Caret(chars[s..i].iter().collect()));
            }
            '~' => {
                i += 1;
                let s = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                out.push(Tk::Tilde(if i > s { Some(chars[s..i].iter().collect()) } else { None }));
            }
            '"' => {
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        s.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    // JavaCC names the token read so far when it is longer than one char.
                    let image: String = chars[token_start..].iter().collect();
                    let after = if chars.len() - token_start > 1 { format!(" after prefix \"{}\"", java_cc_escapes(&image)) } else { String::new() };
                    return Err(parse_error(q, &format!("Lexical error at line 1, column {}.  Encountered: <EOF>{after} (in lexical state 2)", u16_at[chars.len()] + 1)));
                }
                out.push(Tk::Phrase(s));
            }
            '/' => {
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '/' {
                        s.push('/');
                        i += 2;
                        continue;
                    }
                    if chars[i] == '/' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    let prefix: String = chars[token_start..].iter().collect();
                    return Err(parse_error(q, &format!("Lexical error at line 1, column {}.  Encountered: <EOF> after prefix \"{prefix}\" (in lexical state 2)", u16_at[chars.len()] + 1)));
                }
                out.push(Tk::Regex(s));
            }
            '[' | '{' => {
                let il = c == '[';
                i += 1;
                let mut parts: Vec<String> = Vec::new();
                let mut cur = String::new();
                let mut iu = true;
                let mut closed = false;
                let mut quoted = false;
                while i < chars.len() {
                    let ch = chars[i];
                    if ch == '"' {
                        quoted = !quoted;
                        i += 1;
                        continue;
                    }
                    if !quoted && (ch == ']' || ch == '}') {
                        iu = ch == ']';
                        closed = true;
                        i += 1;
                        break;
                    }
                    if !quoted && ch.is_whitespace() {
                        if !cur.is_empty() {
                            parts.push(std::mem::take(&mut cur));
                        }
                        i += 1;
                        continue;
                    }
                    if ch == '\\' && i + 1 < chars.len() {
                        cur.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    cur.push(ch);
                    i += 1;
                }
                if !cur.is_empty() {
                    parts.push(cur);
                }
                // The input ends inside the range: what Lucene's `Term` production wanted next.
                if !closed && (parts.len() != 2 || parts[1] == "TO") && parts.len() <= 3 {
                    let expected: &[u8] = match parts.len() {
                        1 => &[K_RANGE_TO],
                        3 => &[K_RANGEIN_END, K_RANGEEX_END],
                        _ => &[K_RANGE_TO, K_RANGE_QUOTED, K_RANGE_GOOP],
                    };
                    return Err(parse_error(q, &syntax_message(None, u16_at[chars.len()], expected)));
                }
                if !closed || parts.len() != 3 || parts[1] != "TO" {
                    return Err(parse_error(q, "Encountered: range syntax error"));
                }
                out.push(Tk::Range { lo: parts[0].clone(), hi: parts[2].clone(), il, iu });
            }
            _ => {
                let mut s = String::new();
                let mut wild = false;
                while i < chars.len() {
                    let ch = chars[i];
                    if ch == '\\' {
                        if i + 1 < chars.len() {
                            s.push('\\');
                            s.push(chars[i + 1]);
                            i += 2;
                            continue;
                        }
                        return Err(parse_error(q, &format!("Lexical error at line 1, column {}.  Encountered: <EOF> (in lexical state 2)", u16_at[chars.len()] + 1)));
                    }
                    // `_TERM_CHAR` adds `-` and `+` after the first character, but not `!`.
                    if special(ch) && !(ch == '-' && !s.is_empty()) && !(ch == '+' && !s.is_empty()) {
                        break;
                    }
                    if ch == '*' || ch == '?' {
                        wild = true;
                    }
                    s.push(ch);
                    i += 1;
                }
                if i < chars.len() && chars[i] == ':' {
                    out.push(Tk::Field(unescape(&s)));
                    i += 1;
                    continue;
                }
                match s.as_str() {
                    "AND" => out.push(Tk::And),
                    "OR" => out.push(Tk::Or),
                    "NOT" => out.push(Tk::Not),
                    _ => {
                        // `>=x`, `<y`: Elasticsearch's range shortcuts, for text longer than
                        // the operator (a bare `>` is a term; `>=` is `>` of `=`).
                        if s.len() <= 1 {
                            out.push(Tk::Term(s, wild));
                        } else if let Some(rest) = s.strip_prefix(">=").filter(|r| !r.is_empty()) {
                            out.push(Tk::Range { lo: rest.to_string(), hi: "*".into(), il: true, iu: true });
                        } else if let Some(rest) = s.strip_prefix("<=").filter(|r| !r.is_empty()) {
                            out.push(Tk::Range { lo: "*".into(), hi: rest.to_string(), il: true, iu: true });
                        } else if let Some(rest) = s.strip_prefix('>') {
                            out.push(Tk::Range { lo: rest.to_string(), hi: "*".into(), il: false, iu: true });
                        } else if let Some(rest) = s.strip_prefix('<') {
                            out.push(Tk::Range { lo: "*".into(), hi: rest.to_string(), il: true, iu: false });
                        } else {
                            out.push(Tk::Term(s, wild));
                        }
                    }
                }
            }
        }
    }
    spans.resize(out.len(), (u16_at[token_start], u16_at[i.min(chars.len())]));
    Ok((out, spans))
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Conj {
    None,
    And,
    Or,
}

#[derive(Clone, Copy, PartialEq)]
enum Mods {
    None,
    Req,
    Not,
}

/// Where a clause starts, for the tokens a syntax error lists as expected.
#[derive(Clone, Copy, PartialEq)]
enum ClausePos {
    /// The first clause of a query or group.
    First,
    /// After `+`, `-` or `NOT`.
    AfterMods,
    /// After `AND` / `OR`.
    AfterConj,
    /// After another clause, with no operator in between.
    Implicit,
}

struct Parser<'c, 'q> {
    ctx: &'c QueryCtx<'q>,
    qs: &'c QueryString,
    toks: Vec<Tk>,
    pos: usize,
    src: String,
    /// Each token's `(start, end)` in UTF-16 units of the query.
    spans: Vec<(usize, usize)>,
    /// Group nesting depth.
    depth: usize,
    /// Where the clause being parsed starts.
    at: ClausePos,
    /// Whether a `^` boost / a `~` fuzzy slop could still follow the last clause.
    tails: (bool, bool),
    /// Consecutive bare terms in the current query: two or more parse as Lucene's
    /// `MultiTerm`, which takes neither tail.
    bare_run: usize,
}

impl Parser<'_, '_> {
    fn peek(&self) -> Option<&Tk> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tk> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn top(&mut self) -> EsResult<Option<QueryRef>> {
        let start = self.pos;
        let q = self.query(None)?;
        if self.pos < self.toks.len() {
            // Only a `)` stops the top-level query early.
            let expected = if self.pos == start { Self::first_clause() } else { self.after_clause() };
            return Err(self.unexpected(self.pos, &expected));
        }
        Ok(q)
    }

    /// The tokens the first clause of a query or group may start with.
    fn first_clause() -> Vec<u8> {
        let mut e = vec![K_NOT, K_PLUS, K_MINUS];
        e.extend_from_slice(CLAUSE_BODY);
        e.push(K_TERM);
        e
    }

    /// The tokens that may follow a complete clause at the current depth.
    fn after_clause(&self) -> Vec<u8> {
        self.after_clause_with(if self.bare_run >= 2 { (false, false) } else { self.tails })
    }

    fn after_clause_with(&self, (caret, fuzzy): (bool, bool)) -> Vec<u8> {
        let mut e = Vec::with_capacity(20);
        if self.depth == 0 {
            e.push(K_EOF);
        }
        e.extend_from_slice(&[K_AND, K_OR, K_NOT, K_PLUS, K_MINUS, K_BAREOPER, K_LPAREN]);
        if self.depth > 0 {
            e.push(K_RPAREN);
        }
        e.push(K_STAR);
        if caret {
            e.push(K_CARAT);
        }
        e.extend_from_slice(&[K_QUOTED, K_TERM]);
        if fuzzy {
            e.push(K_FUZZY_SLOP);
        }
        e.extend_from_slice(&[K_PREFIXTERM, K_WILDTERM, K_REGEXPTERM, K_RANGEIN_START, K_RANGEEX_START, K_NUMBER]);
        e
    }

    /// The `ParseException` for token `i` (end of input past the last one).
    fn unexpected(&self, i: usize, expected: &[u8]) -> EsError {
        let end = self.spans.last().map_or(0, |s| s.1).max(self.src.encode_utf16().count());
        let msg = match self.toks.get(i) {
            None => syntax_message(None, end, expected),
            Some(t) => {
                let (a, b) = self.spans[i];
                let image: String = String::from_utf16_lossy(&self.src.encode_utf16().skip(a).take(b - a).collect::<Vec<u16>>());
                let kind = match t {
                    Tk::RParen => K_RPAREN,
                    Tk::LParen => K_LPAREN,
                    Tk::And => K_AND,
                    Tk::Or => K_OR,
                    Tk::Not => K_NOT,
                    Tk::Plus => K_PLUS,
                    Tk::Minus => K_MINUS,
                    Tk::Caret(_) => K_CARAT,
                    Tk::Tilde(_) => K_FUZZY_SLOP,
                    Tk::Field(_) => K_TERM,
                    Tk::Phrase(_) => K_QUOTED,
                    Tk::Regex(_) => K_REGEXPTERM,
                    Tk::Range { .. } if image.starts_with('[') => K_RANGEIN_START,
                    Tk::Range { .. } if image.starts_with('{') => K_RANGEEX_START,
                    Tk::Range { .. } => K_TERM,
                    Tk::Term(t, _) if t == "*" => K_STAR,
                    Tk::Term(t, true) if t.ends_with('*') && !t[..t.len() - 1].contains(['*', '?']) => K_PREFIXTERM,
                    Tk::Term(_, true) => K_WILDTERM,
                    Tk::Term(..) => K_TERM,
                };
                // Lucene's `CARAT` and `FUZZY_SLOP` / range-start tokens stop at the operator.
                let image = match t {
                    Tk::Caret(_) => "^".to_string(),
                    Tk::Range { .. } if kind != K_TERM => image[..1].to_string(),
                    Tk::Field(f) => f.clone(),
                    _ => image,
                };
                syntax_message(Some((kind, &image)), a, expected)
            }
        };
        parse_error(&self.src, &msg)
    }

    fn query(&mut self, field: Option<&str>) -> EsResult<Option<QueryRef>> {
        self.bare_run = 0;
        let mut clauses: Vec<(Occur, QueryRef)> = Vec::new();
        let mut first: Option<QueryRef> = None;
        let mut count = 0;
        let mut first_conj = true;
        loop {
            match self.peek() {
                None | Some(Tk::RParen) => break,
                _ => {}
            }
            // A `^` or `~` the last clause can't take: the loop ends there, and the caller
            // wanted whatever may follow a clause.
            if !first_conj && matches!(self.peek(), Some(Tk::Caret(_) | Tk::Tilde(_))) {
                return Err(self.unexpected(self.pos, &self.after_clause()));
            }
            let conj = if first_conj {
                Conj::None
            } else {
                match self.peek() {
                    Some(Tk::And) => {
                        self.pos += 1;
                        Conj::And
                    }
                    Some(Tk::Or) => {
                        self.pos += 1;
                        Conj::Or
                    }
                    _ => Conj::None,
                }
            };
            let mods = match self.peek() {
                Some(Tk::Plus) => {
                    self.pos += 1;
                    Mods::Req
                }
                Some(Tk::Minus) | Some(Tk::Not) => {
                    self.pos += 1;
                    Mods::Not
                }
                _ => Mods::None,
            };
            self.at = if mods != Mods::None {
                ClausePos::AfterMods
            } else if conj != Conj::None {
                ClausePos::AfterConj
            } else if first_conj {
                ClausePos::First
            } else {
                ClausePos::Implicit
            };
            let body = self.pos;
            let q = self.clause(field)?;
            let bare = mods == Mods::None
                && conj == Conj::None
                && self.pos == body + 1
                && matches!(&self.toks[body], Tk::Term(t, false) if t != "*");
            self.bare_run = if bare { self.bare_run + 1 } else { 0 };
            self.add_clause(&mut clauses, conj, mods, q.clone());
            if first_conj && mods == Mods::None {
                first = q;
            }
            count += 1;
            first_conj = false;
        }
        if count == 1
            && let Some(f) = first
        {
            return Ok(Some(f));
        }
        if clauses.is_empty() {
            return Ok(None);
        }
        let mut b = BooleanQuery::new();
        let all_neg = clauses.iter().all(|(o, _)| *o == Occur::MustNot);
        for (o, q) in clauses {
            b = b.add(o, q);
        }
        if all_neg {
            b = b.add(Occur::Filter, arc(search_index::query::MatchAllQuery));
        }
        Ok(Some(arc(b)))
    }

    fn add_clause(&self, clauses: &mut Vec<(Occur, QueryRef)>, conj: Conj, mods: Mods, q: Option<QueryRef>) {
        let and_default = self.qs.default_operator == Operator::And;
        if let Some(last) = clauses.last_mut() {
            if conj == Conj::And && last.0 != Occur::MustNot {
                last.0 = Occur::Must;
            }
            if and_default && conj == Conj::Or && last.0 != Occur::MustNot {
                last.0 = Occur::Should;
            }
        }
        let Some(q) = q else { return };
        let (required, prohibited) = if !and_default {
            let p = mods == Mods::Not;
            let r = mods == Mods::Req || (conj == Conj::And && !p);
            (r, p)
        } else {
            let p = mods == Mods::Not;
            (!p && conj != Conj::Or, p)
        };
        let occur = if required && !prohibited {
            Occur::Must
        } else if !prohibited {
            Occur::Should
        } else {
            Occur::MustNot
        };
        clauses.push((occur, q));
    }

    fn boost(&mut self, q: Option<QueryRef>) -> EsResult<Option<QueryRef>> {
        let b = self.caret()?;
        self.apply_boost(q, b)
    }

    /// An optional `^NUMBER`: the boost.
    fn caret(&mut self) -> EsResult<Option<f32>> {
        if let Some(Tk::Caret(b)) = self.peek().cloned() {
            self.pos += 1;
            // `^` at the end: the parser wanted the boost's `NUMBER`.
            if b.is_empty() && self.pos == self.toks.len() && self.spans.get(self.pos - 1).is_some_and(|s| s.1 == self.src.encode_utf16().count()) {
                return Err(self.unexpected(self.pos, &[K_NUMBER]));
            }
            let v: f32 = b.parse().map_err(|_| parse_error(&self.src, "Encountered \"^\""))?;
            return Ok(Some(v));
        }
        Ok(None)
    }

    fn apply_boost(&self, q: Option<QueryRef>, boost: Option<f32>) -> EsResult<Option<QueryRef>> {
        Ok(match boost {
            Some(v) if v != 1.0 => q.map(|q| arc(BoostQuery::new(q, v)) as QueryRef),
            _ => q,
        })
    }

    /// A term's or phrase's tails, `[^n [~]] | [~ [^n]]` (read before the query is built, as
    /// Lucene's `Term` production does): its fuzzy slop (`Some(None)` for a bare `~`) and boost.
    fn tails_of_term(&mut self) -> EsResult<(Option<Option<String>>, Option<f32>)> {
        if let Some(Tk::Tilde(n)) = self.peek().cloned() {
            self.pos += 1;
            return Ok((Some(n), self.caret()?));
        }
        let boost = self.caret()?;
        let mut fuzzy = None;
        if boost.is_some()
            && let Some(Tk::Tilde(n)) = self.peek().cloned()
        {
            self.pos += 1;
            fuzzy = Some(n);
        }
        Ok((fuzzy, boost))
    }

    /// The `ParseException` for a token that can't start the clause body at `i`.
    fn bad_clause_start(&self, i: usize, prefixed: bool) -> EsError {
        // A `field:` token where the body goes: Lucene read the name as the body's term and
        // trips over the colon.
        if let Some(Tk::Field(_)) = self.toks.get(i) {
            let (_, b) = self.spans[i];
            let expected = self.after_clause_with((true, true));
            let msg = syntax_message(Some((K_COLON, ":")), b - 1, &expected);
            return parse_error(&self.src, &msg);
        }
        let mut expected = Vec::with_capacity(16);
        if !prefixed && matches!(self.at, ClausePos::First | ClausePos::AfterConj | ClausePos::Implicit) {
            if self.at == ClausePos::Implicit {
                expected.extend_from_slice(&[K_AND, K_OR]);
            }
            expected.extend_from_slice(&[K_NOT, K_PLUS, K_MINUS]);
        }
        expected.extend_from_slice(CLAUSE_BODY);
        if !prefixed {
            // The `field:` lookaheads (`<TERM> ":"`, `"*" ":"`).
            expected.push(K_TERM);
            if self.at != ClausePos::First {
                expected.push(K_STAR);
            }
        }
        self.unexpected(i, &expected)
    }

    fn clause(&mut self, field: Option<&str>) -> EsResult<Option<QueryRef>> {
        let mut field: Option<String> = field.map(str::to_string);
        let mut prefixed = false;
        if let Some(Tk::Field(f)) = self.peek().cloned() {
            self.pos += 1;
            field = Some(f);
            prefixed = true;
        }
        let body = self.pos;
        let r = self.clause_body(field.as_deref(), prefixed)?;
        // What may still follow: Lucene's `Term` takes `[^n [~]] | [~ [^n]]` after a term or
        // a phrase, `[^n]` after a range or group.
        let tail = &self.toks[(body + 1).min(self.pos)..self.pos];
        let caret = tail.iter().any(|t| matches!(t, Tk::Caret(_)));
        let tilde = tail.iter().any(|t| matches!(t, Tk::Tilde(_)));
        self.tails = match &self.toks[body] {
            Tk::Term(..) | Tk::Regex(_) | Tk::Phrase(_) => (!caret, !tilde),
            _ => (!matches!(self.toks.get(self.pos - 1), Some(Tk::Caret(_))), false),
        };
        Ok(r)
    }

    fn clause_body(&mut self, field: Option<&str>, prefixed: bool) -> EsResult<Option<QueryRef>> {
        let field: Option<String> = field.map(str::to_string);
        match self.next() {
            Some(Tk::LParen) => {
                self.depth += 1;
                let start = self.pos;
                let q = self.query(field.as_deref())?;
                match self.peek() {
                    // A group holds at least one clause.
                    Some(Tk::RParen) if self.pos > start => self.pos += 1,
                    // The group's query stops only at `)` or the end.
                    _ => {
                        let expected = if self.pos == start { Self::first_clause() } else { self.after_clause() };
                        return Err(self.unexpected(self.pos, &expected));
                    }
                }
                self.depth -= 1;
                self.boost(q)
            }
            Some(Tk::Term(t, wild)) => {
                let (fuzzy, boost) = self.tails_of_term()?;
                let q = self.term(field.as_deref(), &t, wild, fuzzy)?;
                self.apply_boost(q, boost)
            }
            Some(Tk::Phrase(p)) => {
                let (fuzzy, boost) = self.tails_of_term()?;
                let slop = fuzzy.flatten().and_then(|s| s.parse::<f32>().ok()).map(|f| f as i32).unwrap_or(self.qs.phrase_slop);
                let q = self.phrase(field.as_deref(), &p, slop)?;
                self.apply_boost(q, boost)
            }
            Some(Tk::Regex(r)) => {
                // A regex takes a term's tails; its fuzzy slop means nothing.
                let (_, boost) = self.tails_of_term()?;
                // The length limit is not lenient.
                self.ctx.check_regex_length(&r, "Regexp")?;
                let q = self.each_field(field.as_deref(), &mut |ctx, f| {
                    // OpenSearch 3.x normalizes the regex with the text field's analyzer
                    // (`/UPPERCASE/` finds `uppercase`); Elasticsearch keeps it as written.
                    let value = match ctx.field(f) {
                        Some(fi) if crate::opensearch::active() && fi.kind.is_text() => match fi.query_analyzer() {
                            Some(a) => a.normalize(&r),
                            None => r.clone(),
                        },
                        _ => r.clone(),
                    };
                    let rq = crate::query::term_level::Regexp {
                        field: f.to_string(),
                        value,
                        flags: crate::query::term_level::regexp_flags::LUCENE_ALL,
                        max_determinized_states: 10000,
                        rewrite: None,
                        case_insensitive: false,
                        common: Default::default(),
                    };
                    ctx.to_query(&crate::query::Query::Regexp(Box::new(rq))).map(Some)
                })?;
                self.apply_boost(q, boost)
            }
            Some(Tk::Range { lo, hi, il, iu }) => {
                let q = self.each_field(field.as_deref(), &mut |ctx, f| {
                    let v = |s: &str| if s == "*" { None } else { Some(Value::Str(s.to_string())) };
                    let r = crate::query::term_level::Range {
                        field: f.to_string(),
                        from: v(&lo),
                        to: v(&hi),
                        include_lower: il,
                        include_upper: iu,
                        time_zone: self.qs.time_zone.clone(),
                        format: None,
                        relation: None,
                        common: Default::default(),
                    };
                    super::range::range_query(ctx, &r).map(Some)
                })?;
                self.boost(q)
            }
            Some(_) | None => Err(self.bad_clause_start(self.pos - 1, prefixed)),
        }
    }

    /// The fields a clause runs on (its own, or the default ones), with boosts, in
    /// `HashMap` order (see [`text::hash_ordered`]).
    fn fields_of(&self, field: Option<&str>) -> Vec<(String, f32)> {
        text::hash_ordered(self.fields_unordered(field))
    }

    fn fields_unordered(&self, field: Option<&str>) -> Vec<(String, f32)> {
        match field {
            Some(f) => {
                if f.contains('*') {
                    text::resolve_fields(self.ctx, &[(f.to_string(), 1.0)])
                } else {
                    vec![(f.to_string(), 1.0)]
                }
            }
            None => {
                if !self.qs.fields.is_empty() {
                    text::resolve_fields(self.ctx, &self.qs.fields)
                } else if let Some(df) = &self.qs.default_field {
                    text::resolve_fields(self.ctx, &[parse_field_boost(df)])
                } else {
                    text::resolve_fields(self.ctx, &text::default_fields(self.ctx))
                }
            }
        }
    }

    fn lenient(&self, field: Option<&str>) -> bool {
        // `QueryStringQueryBuilder.doToQuery`: all-fields mode (`default_field: *`, a `*` among
        // `fields`, or the index's default fields holding `*`) is lenient by default, for
        // every clause, its own fields included; otherwise `index.query_string.lenient`.
        let _ = field;
        self.qs.lenient.unwrap_or_else(|| {
            let all_fields = match &self.qs.default_field {
                Some(d) => d == "*",
                None if !self.qs.fields.is_empty() => self.qs.fields.iter().any(|(f, _)| f == "*"),
                None => text::default_fields(self.ctx).iter().any(|(f, _)| f == "*"),
            };
            all_fields || self.ctx.idx.settings.get("index.query_string.lenient") == Some("true")
        })
    }

    fn each_field(&self, field: Option<&str>, f: &mut dyn FnMut(&QueryCtx, &str) -> EsResult<Option<QueryRef>>) -> EsResult<Option<QueryRef>> {
        let fields = self.fields_of(field);
        let lenient = self.lenient(field);
        let mut qs = Vec::new();
        for (name, boost) in fields {
            match f(self.ctx, &name) {
                Ok(Some(q)) => qs.push(if boost == 1.0 { q } else { arc(BoostQuery::new(q, boost)) }),
                Ok(None) => {}
                Err(e) if lenient => qs.push(text::lenient_none(&name, &e)),
                Err(e) => return Err(e),
            }
        }
        Ok(match qs.len() {
            0 => None,
            1 => qs.pop(),
            _ => Some(arc(DisMaxQuery::new(qs, self.qs.tie_breaker.unwrap_or(0.0)))),
        })
    }

    fn term(&mut self, field: Option<&str>, raw: &str, wild: bool, fuzzy: Option<Option<String>>) -> EsResult<Option<QueryRef>> {
        if field == Some("_exists_") {
            return self.ctx.exists_query(&unescape(raw)).map(Some);
        }
        if wild && (raw == "*") {
            // `QueryStringQueryParser.getWildcardQuery`: `*` on a field (the clause's, or the
            // parser's single default field) is that field's `exists` query, on the
            // all-fields pattern every document; with several default fields each gets its
            // own `exists`.
            let actual = match field {
                Some(f) => Some(f.to_string()),
                None if !self.qs.fields.is_empty() => None,
                None => match &self.qs.default_field {
                    Some(df) => Some(df.clone()),
                    None => {
                        let defaults = text::default_fields(self.ctx);
                        if defaults.iter().any(|(f, _)| f == "*") { Some("*".to_string()) } else { None }
                    }
                },
            };
            return match actual {
                Some(f) if f.contains('*') && f.chars().all(|c| c == '*') => Ok(Some(arc(search_index::query::MatchAllQuery))),
                Some(f) => self.star_exists(&f).map(Some),
                None => self.each_field(None, &mut |ctx, f| {
                    let _ = ctx;
                    self.star_exists(f).map(Some)
                }),
            };
        }
        if wild {
            let allow_leading = self.qs.allow_leading_wildcard.unwrap_or(true);
            if !allow_leading && (raw.starts_with('*') || raw.starts_with('?')) {
                return Err(parse_error(&self.src, "'*' or '?' not allowed as first character in WildcardQuery"));
            }
            let pattern = raw.to_string();
            let is_prefix = pattern.ends_with('*') && !pattern[..pattern.len() - 1].contains(['*', '?']);
            return self.each_field(field, &mut |ctx, f| {
                let fi = ctx.field(f);
                let norm = |s: &str| -> String {
                    match fi.as_ref().and_then(|fi| fi.query_analyzer().cloned()) {
                        Some(a) if fi.as_ref().is_some_and(|x| x.kind.is_text()) => a.normalize(s),
                        _ => s.to_string(),
                    }
                };
                if is_prefix {
                    let p = norm(&unescape(&pattern[..pattern.len() - 1]));
                    ctx.prefix_query(f, &p, false, self.qs.rewrite.as_deref()).map(Some)
                } else {
                    let p = normalize_pattern(&pattern, &norm);
                    ctx.wildcard_query(f, &p, false, self.qs.rewrite.as_deref()).map(Some)
                }
            });
        }
        let term = unescape(raw);
        if let Some(fz) = fuzzy {
            let fuzziness = match fz {
                None => self.qs.fuzziness.clone(),
                Some(n) => {
                    let v: f32 = n.parse().unwrap_or(2.0);
                    if v >= 1.0 || v == 0.0 { Fuzziness::Edits(v.min(2.0) as u8) } else { Fuzziness::AUTO }
                }
            };
            return self.each_field(field, &mut |ctx, f| {
                let fi = ctx.field(f);
                let t = match fi.as_ref().and_then(|fi| fi.query_analyzer().cloned()) {
                    Some(a) if fi.as_ref().is_some_and(|x| x.kind.is_text()) => a.normalize(&term),
                    _ => term.clone(),
                };
                let fq = crate::query::term_level::Fuzzy {
                    field: f.to_string(),
                    value: Value::Str(t),
                    fuzziness: fuzziness.clone(),
                    prefix_length: self.qs.fuzzy_prefix_length,
                    max_expansions: self.qs.fuzzy_max_expansions,
                    transpositions: self.qs.fuzzy_transpositions,
                    rewrite: self.qs.fuzzy_rewrite.clone(),
                    common: Default::default(),
                };
                ctx.to_query(&crate::query::Query::Fuzzy(Box::new(fq))).map(Some)
            });
        }
        self.field_text(field, &term, false, 0)
    }

    /// `ExistsQueryBuilder.newFilter` as the query string parser calls it: an unmapped field
    /// matches nothing (`unmapped field:<name>`), not a rewrite to `match_none`.
    fn star_exists(&self, f: &str) -> EsResult<QueryRef> {
        let q = self.ctx.exists_query(f)?;
        if super::query::rewritten_to_none(&q) {
            return Ok(super::query::match_none(&format!("unmapped field:{f}")));
        }
        Ok(q)
    }

    fn phrase(&mut self, field: Option<&str>, text: &str, slop: i32) -> EsResult<Option<QueryRef>> {
        self.field_text(field, text, true, slop)
    }

    /// Analyzed text on the clause's fields (`getFieldQuery`).
    fn field_text(&mut self, field: Option<&str>, text: &str, quoted: bool, slop: i32) -> EsResult<Option<QueryRef>> {
        let fields = self.fields_of(field);
        let lenient = self.lenient(field);
        let quote_fields: Vec<(String, f32)> = match (&self.qs.quote_field_suffix, quoted) {
            (Some(sfx), true) => fields.iter().map(|(f, b)| {
                let qf = format!("{f}{sfx}");
                if self.ctx.field(&qf).is_some() { (qf, *b) } else { (f.clone(), *b) }
            }).collect(),
            _ => fields,
        };
        let analyzer = if quoted { self.qs.quote_analyzer.as_deref().or(self.qs.analyzer.as_deref()) } else { self.qs.analyzer.as_deref() };
        let o = MatchOpts {
            operator: self.qs.default_operator,
            analyzer,
            fuzzy: None,
            slop,
            lenient,
            auto_generate_synonyms_phrase_query: self.qs.auto_generate_synonyms_phrase_query,
            ..Default::default()
        };
        let ty = if quoted { MatchType::Phrase } else { MatchType::Boolean };
        let mut qs = Vec::new();
        for (name, boost) in &quote_fields {
            let r = text::field_query(self.ctx, name, &Value::Str(text.to_string()), ty, &o, "query_string");
            match r {
                Ok(Some(q)) => {
                    qs.push(if *boost == 1.0 { q } else { arc(BoostQuery::new(q, *boost)) as QueryRef });
                }
                Ok(None) => {}
                Err(e) if lenient => qs.push(text::lenient_none(name, &e)),
                Err(e) => return Err(e),
            }
        }
        let tie = self.qs.tie_breaker.unwrap_or(if matches!(self.qs.ty, MultiMatchType::MostFields | MultiMatchType::BoolPrefix) { 1.0 } else { 0.0 });
        Ok(match qs.len() {
            0 => None,
            1 => qs.pop(),
            _ => Some(arc(DisMaxQuery::new(qs, tie))),
        })
    }
}

fn normalize_pattern(p: &str, norm: &dyn Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut lit = String::new();
    let mut chars = p.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' | '?' => {
                if !lit.is_empty() {
                    out.push_str(&super::query::escape_wildcard(&norm(&lit)));
                    lit.clear();
                }
                out.push(c);
            }
            '\\' => {
                if let Some(n) = chars.next() {
                    lit.push(n);
                }
            }
            c => lit.push(c),
        }
    }
    if !lit.is_empty() {
        out.push_str(&super::query::escape_wildcard(&norm(&lit)));
    }
    out
}


/// `query_string`.
pub fn query_string(ctx: &QueryCtx, qs: &QueryString) -> EsResult<QueryRef> {
    let wrap = |e: EsError| -> EsError {
        if e.kind == "parse_exception" {
            super::query::qse(ctx, format!("Failed to parse query [{}]", qs.query)).caused_by(e)
        } else {
            e
        }
    };
    let (toks, spans) = tokenize_spanned(&qs.query).map_err(wrap)?;
    let mut p = Parser { ctx, qs, toks, pos: 0, src: qs.query.clone(), spans, depth: 0, at: ClausePos::First, tails: (true, true), bare_run: 0 };
    let q = p.top().map_err(wrap)?;
    let q = match q {
        Some(q) => q,
        // `QueryParserBase.parse`: a query without clauses is an empty `BooleanQuery`, which
        // matches nothing (and prints as nothing).
        None => return Ok(arc(BooleanQuery::new())),
    };
    text::apply_msm(q, qs.minimum_should_match.as_deref())
}

/// `simple_query_string`: `+` (and), `|` (or), `-` (not), `"phrase"`, `prefix*`, `(…)`, `~N`;
/// whitespace is the default operator. Never fails on syntax.
pub fn simple_query_string(ctx: &QueryCtx, s: &SimpleQueryString) -> EsResult<QueryRef> {
    let explicit = !s.fields.is_empty();
    let fields = if explicit { text::resolve_fields(ctx, &s.fields) } else { text::resolve_fields(ctx, &text::default_fields(ctx)) };
    // The resolved fields are a `HashMap`: its iteration order orders the per-field queries.
    let fields = text::hash_ordered(fields);
    let lenient = s.lenient.unwrap_or(!explicit);
    let flags = if s.flags == sqs_flags::ALL { i32::MAX } else { s.flags };
    let mut sp = Simple { ctx, s, fields, lenient, flags, chars: s.query.chars().collect() };
    let q = if s.query.trim() == "*" { Some(arc(search_index::query::MatchAllQuery) as QueryRef) } else { sp.parse_range(0, sp.chars.len())? };
    let q = match q {
        Some(q) => q,
        None => return Ok(match_none("Matching no documents because no terms present")),
    };
    text::apply_msm(q, s.minimum_should_match.as_deref())
}

/// Parser state of one (sub-)query range.
struct SqState {
    index: usize,
    end: usize,
    top: Option<QueryRef>,
    current: Option<Occur>,
    previous: Option<Occur>,
    not: u32,
}

struct Simple<'c, 'q> {
    ctx: &'c QueryCtx<'q>,
    s: &'c SimpleQueryString,
    fields: Vec<(String, f32)>,
    lenient: bool,
    flags: i32,
    chars: Vec<char>,
}

const F_AND: i32 = 1;
const F_NOT: i32 = 1 << 1;
const F_OR: i32 = 1 << 2;
const F_PREFIX: i32 = 1 << 3;
const F_PHRASE: i32 = 1 << 4;
const F_PRECEDENCE: i32 = 1 << 5;
const F_ESCAPE: i32 = 1 << 6;
const F_WHITESPACE: i32 = 1 << 7;
const F_FUZZY: i32 = 1 << 8;
const F_NEAR: i32 = 1 << 9;

impl Simple<'_, '_> {
    fn on(&self, f: i32) -> bool {
        self.flags & f != 0
    }

    /// Lucene's `SimpleQueryParser.parseSubQuery`: terms, phrases and sub-queries folded
    /// into a tree left to right; an explicit `+`/`|` applies to the next branch only, and a
    /// change of operator wraps the tree so far as a clause of a new parent.
    fn parse_range(&mut self, start: usize, end: usize) -> EsResult<Option<QueryRef>> {
        let mut st = SqState { index: start, end, top: None, current: None, previous: None, not: 0 };
        while st.index < st.end {
            let c = self.chars[st.index];
            if c == '(' && self.on(F_PRECEDENCE) {
                self.consume_sub_query(&mut st)?;
            } else if c == ')' && self.on(F_PRECEDENCE) {
                st.index += 1;
            } else if c == '"' && self.on(F_PHRASE) {
                self.consume_phrase(&mut st)?;
            } else if c == '+' && self.on(F_AND) {
                if st.current.is_none() && st.top.is_some() {
                    st.current = Some(Occur::Must);
                }
                st.index += 1;
            } else if c == '|' && self.on(F_OR) {
                if st.current.is_none() && st.top.is_some() {
                    st.current = Some(Occur::Should);
                }
                st.index += 1;
            } else if c == '-' && self.on(F_NOT) {
                st.not += 1;
                st.index += 1;
                continue;
            } else if matches!(c, ' ' | '\t' | '\n' | '\r') && self.on(F_WHITESPACE) {
                st.index += 1;
            } else {
                self.consume_token(&mut st)?;
            }
            st.not = 0;
        }
        Ok(st.top)
    }

    fn consume_sub_query(&mut self, st: &mut SqState) -> EsResult<()> {
        st.index += 1;
        let start = st.index;
        let mut depth = 1;
        let mut escaped = false;
        while st.index < st.end {
            let c = self.chars[st.index];
            if !escaped {
                if c == '\\' && self.on(F_ESCAPE) {
                    escaped = true;
                    st.index += 1;
                    continue;
                } else if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            escaped = false;
            st.index += 1;
        }
        if st.index == st.end {
            // No closing parenthesis: the opening one is ignored.
            st.index = start;
        } else if st.index == start {
            st.current = None;
            st.index += 1;
        } else {
            let sub = self.parse_range(start, st.index)?;
            self.build_tree(st, sub);
            st.index += 1;
        }
        Ok(())
    }

    fn consume_phrase(&mut self, st: &mut SqState) -> EsResult<()> {
        st.index += 1;
        let start = st.index;
        let mut text = String::new();
        let mut escaped = false;
        let mut has_slop = false;
        while st.index < st.end {
            let c = self.chars[st.index];
            if !escaped {
                if c == '\\' && self.on(F_ESCAPE) {
                    escaped = true;
                    st.index += 1;
                    continue;
                } else if c == '"' {
                    if st.index + 1 < st.end && self.chars[st.index + 1] == '~' && self.on(F_NEAR) {
                        st.index += 1;
                        has_slop = st.index + 1 < st.end;
                    }
                    break;
                }
            }
            escaped = false;
            text.push(c);
            st.index += 1;
        }
        // The slop is read only once the phrase is known to be closed (`consumePhrase`).
        if st.index == st.end {
            // No closing quote: the opening one is ignored.
            st.index = start;
        } else if text.is_empty() {
            st.current = None;
            st.index += 1;
        } else {
            let slop = if has_slop { self.parse_fuzziness(st) } else { 0 };
            let q = self.text_query(&text, true, slop)?;
            self.build_tree(st, q);
            st.index += 1;
        }
        Ok(())
    }

    fn token_finished(&self, c: char) -> bool {
        (c == '"' && self.on(F_PHRASE))
            || (c == '|' && self.on(F_OR))
            || (c == '+' && self.on(F_AND))
            || ((c == '(' || c == ')') && self.on(F_PRECEDENCE))
            || (matches!(c, ' ' | '\t' | '\n' | '\r') && self.on(F_WHITESPACE))
    }

    fn consume_token(&mut self, st: &mut SqState) -> EsResult<()> {
        let mut t = String::new();
        let mut escaped = false;
        let mut prefix = false;
        let mut fuzzy = false;
        while st.index < st.end {
            let c = self.chars[st.index];
            if !escaped {
                if c == '\\' && self.on(F_ESCAPE) {
                    escaped = true;
                    prefix = false;
                    st.index += 1;
                    continue;
                } else if self.token_finished(c) {
                    break;
                } else if !t.is_empty() && c == '~' && self.on(F_FUZZY) {
                    fuzzy = true;
                    break;
                }
                prefix = !t.is_empty() && c == '*' && self.on(F_PREFIX);
            }
            escaped = false;
            t.push(c);
            st.index += 1;
        }
        if t.is_empty() {
            return Ok(());
        }
        let q = if fuzzy {
            let n = self.parse_fuzziness(st).min(2);
            if n == 0 { self.text_query(&t, false, 0)? } else { self.fuzzy_query(&t, n as u8)? }
        } else if prefix {
            t.pop();
            self.prefix(&t)?
        } else {
            self.text_query(&t, false, 0)?
        };
        self.build_tree(st, q);
        Ok(())
    }

    /// `~N` after a term or phrase; `~` alone is 2, unparsable or negative is 0.
    fn parse_fuzziness(&self, st: &mut SqState) -> i32 {
        if self.chars.get(st.index) != Some(&'~') {
            return 0;
        }
        let mut text = String::new();
        while st.index < st.end {
            st.index += 1;
            if st.index < st.end {
                let c = self.chars[st.index];
                if self.token_finished(c) {
                    break;
                }
                text.push(c);
            }
        }
        if text.is_empty() {
            return 2;
        }
        text.parse::<i32>().unwrap_or(0).max(0)
    }

    fn build_tree(&self, st: &mut SqState, branch: Option<QueryRef>) {
        let Some(mut branch) = branch else { return };
        if st.not % 2 == 1 {
            branch = arc(BooleanQuery::new().add(Occur::MustNot, branch).add(Occur::Should, arc(search_index::query::MatchAllQuery)));
        }
        match st.top.take() {
            None => st.top = Some(branch),
            Some(top) => {
                let default = if self.s.default_operator == Operator::And { Occur::Must } else { Occur::Should };
                let op = *st.current.get_or_insert(default);
                let parent = if st.previous != Some(op) {
                    BooleanQuery::new().add(op, top)
                } else {
                    match top.as_any().downcast_ref::<BooleanQuery>() {
                        Some(b) => b.clone(),
                        None => BooleanQuery::new().add(op, top),
                    }
                };
                st.top = Some(arc(parent.add(op, branch)));
                st.previous = Some(op);
            }
        }
        st.current = None;
    }

    fn per_field(&self, f: &mut dyn FnMut(&str) -> EsResult<Option<QueryRef>>) -> EsResult<Option<QueryRef>> {
        let mut qs = Vec::new();
        for (name, boost) in &self.fields {
            match f(name) {
                Ok(Some(q)) => qs.push(if *boost == 1.0 { q } else { arc(BoostQuery::new(q, *boost)) as QueryRef }),
                Ok(None) => {}
                Err(e) if self.lenient => qs.push(text::lenient_none(name, &e)),
                Err(e) => return Err(e),
            }
        }
        // `SimpleQueryStringQueryParser`: one field's query alone, several under a
        // `DisjunctionMaxQuery` with tie breaker 1.0.
        Ok(match qs.len() {
            0 => None,
            1 => qs.pop(),
            _ => Some(arc(DisMaxQuery::new(qs, 1.0))),
        })
    }

    fn text_query(&self, t: &str, quoted: bool, slop: i32) -> EsResult<Option<QueryRef>> {
        let o = MatchOpts {
            operator: self.s.default_operator,
            analyzer: self.s.analyzer.as_deref(),
            slop,
            lenient: self.lenient,
            auto_generate_synonyms_phrase_query: self.s.auto_generate_synonyms_phrase_query,
            ..Default::default()
        };
        let ty = if quoted { MatchType::Phrase } else { MatchType::Boolean };
        self.per_field(&mut |name| {
            let n = match (&self.s.quote_field_suffix, quoted) {
                (Some(sfx), true) if self.ctx.field(&format!("{name}{sfx}")).is_some() => format!("{name}{sfx}"),
                _ => name.to_string(),
            };
            text::field_query(self.ctx, &n, &Value::Str(t.to_string()), ty, &o, "simple_query_string")
        })
    }

    fn prefix(&self, t: &str) -> EsResult<Option<QueryRef>> {
        self.per_field(&mut |name| {
            let fi = self.ctx.field(name);
            let p = match fi.as_ref().and_then(|f| f.query_analyzer().cloned()) {
                Some(a) if fi.as_ref().is_some_and(|x| x.kind.is_text()) => a.normalize(t),
                _ => t.to_string(),
            };
            if fi.as_ref().is_some_and(|f| !f.kind.is_text() && !f.kind.is_keyword_like()) {
                return if self.lenient { Ok(None) } else { self.ctx.prefix_query(name, &p, false, None).map(Some) };
            }
            self.ctx.prefix_query(name, &p, false, None).map(Some)
        })
    }

    fn fuzzy_query(&self, t: &str, edits: u8) -> EsResult<Option<QueryRef>> {
        self.per_field(&mut |name| {
            let fi = self.ctx.field(name);
            let v = match fi.as_ref().and_then(|f| f.query_analyzer().cloned()) {
                Some(a) if fi.as_ref().is_some_and(|x| x.kind.is_text()) => a.normalize(t),
                _ => t.to_string(),
            };
            let fq = crate::query::term_level::Fuzzy {
                field: name.to_string(),
                value: Value::Str(v),
                fuzziness: Fuzziness::Edits(edits.min(2)),
                prefix_length: self.s.fuzzy_prefix_length,
                max_expansions: self.s.fuzzy_max_expansions,
                transpositions: self.s.fuzzy_transpositions,
                rewrite: None,
                common: Default::default(),
            };
            self.ctx.to_query(&crate::query::Query::Fuzzy(Box::new(fq))).map(Some)
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn syntax_messages_like_javacc() {
        // Captured from Elasticsearch 9.5.5 (`q=kw1:` and `q=a)`).
        assert_eq!(
            syntax_message(None, 4, CLAUSE_BODY),
            "Encountered \"<EOF>\" at line 1, column 4.\nWas expecting one of:\n    <BAREOPER> ...\n    \"(\" ...\n    \"*\" ...\n    <QUOTED> ...\n    <TERM> ...\n    <PREFIXTERM> ...\n    <WILDTERM> ...\n    <REGEXPTERM> ...\n    \"[\" ...\n    \"{\" ...\n    <NUMBER> ...\n    "
        );
        assert_eq!(syntax_message(Some((K_RPAREN, ")")), 1, &[K_EOF, K_AND]), "Encountered \" \")\" \") \"\" at line 1, column 1.\nWas expecting one of:\n    <EOF> \n    <AND> ...\n    ");
        assert_eq!(syntax_message(None, 2, &[K_NUMBER]), "Encountered \"<EOF>\" at line 1, column 2.\nWas expecting:\n    <NUMBER> ...\n    ");
        assert_eq!(java_cc_escapes("é\"\\"), "\\u00e9\\\"\\\\");
    }

    #[test]
    fn token_spans_count_utf16_units() {
        let (toks, spans) = tokenize_spanned("é) f:x \"a b\"").unwrap();
        assert_eq!(toks.len(), 5);
        assert_eq!(spans, vec![(0, 1), (1, 2), (3, 5), (5, 6), (7, 12)]);
        let err = parse_error("a)", "m");
        assert!(!err.es && err.caused_by.as_ref().is_some_and(|c| !c.es && c.kind == "parse_exception"));
        assert_eq!(parse_error("x", "Lexical error at line 1").caused_by.unwrap().kind, "token_mgr_error");
    }

    use super::*;

    #[test]
    fn tokenizes_the_classic_syntax() {
        let t = tokenize("title:(quick OR brown) +fox -\"lazy dog\"~2 ab* /re.ex/ [1 TO 5] >=10 x^2").unwrap();
        assert_eq!(t[0], Tk::Field("title".into()));
        assert!(t.contains(&Tk::Phrase("lazy dog".into())));
        assert!(t.contains(&Tk::Tilde(Some("2".into()))));
        assert!(t.contains(&Tk::Term("ab*".into(), true)));
        assert!(t.contains(&Tk::Regex("re.ex".into())));
        assert!(t.contains(&Tk::Range { lo: "1".into(), hi: "5".into(), il: true, iu: true }));
        assert!(t.contains(&Tk::Range { lo: "10".into(), hi: "*".into(), il: true, iu: true }));
        assert!(t.contains(&Tk::Caret("2".into())));
        assert!(tokenize("\"open").is_err());
        assert_eq!(parse_field_boost("title^3"), ("title".into(), 3.0));
    }
}
