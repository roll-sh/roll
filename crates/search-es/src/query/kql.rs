//! `kql`: the Kibana Query Language as a query (Elasticsearch's x-pack `kql` plugin).
//!
//! The builder keeps the text; it is parsed into a [`Node`] tree here and turned into other
//! queries against the shard's mapping in the search phase (`search::kql`), as Elasticsearch
//! does in `KqlQueryBuilder.doRewrite`. Syntax errors are `parsing_exception`s
//! (`KqlParsingException`, `line l:c: …`).
//!
//! Grammar: `a and b`, `a or b`, `not a` (keywords in any case, `and` binding tighter than
//! `or`), parentheses, `field: value`, `field: "phrase"`, `field: (v1 or v2)`, `field < v`
//! (`<=`, `>`, `>=`), `field: *` (exists), `path: { … }` (nested), `*` / `*:*` (everything),
//! bare values (every field), `*` wildcards in values and field names, `\` escapes, and an
//! empty query matching everything.

use super::xparser::{Field, Tokens};
use super::{Common, XParser, check_boost};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};

/// `kql`.
#[derive(Clone, Debug, PartialEq)]
pub struct Kql {
    pub query: String,
    pub case_insensitive: bool,
    pub default_field: Option<String>,
    pub time_zone: Option<String>,
    pub common: Common,
}

impl Kql {
    pub fn parse(p: &mut XParser<'_>) -> EsResult<Kql> {
        const FIELDS: &[Field] = &[
            Field::new("query", Tokens::STRING),
            Field::new("case_insensitive", Tokens::BOOLEAN),
            Field::new("default_field", Tokens::STRING),
            Field::new("time_zone", Tokens::STRING),
            Field::new("boost", Tokens::FLOAT),
            Field::new("_name", Tokens::STRING),
        ];
        let (mut query, mut ci, mut df, mut tz, mut boost, mut name) = (None, false, None, None, None, None);
        p.object("kql", FIELDS, |p, i| {
            match i {
                0 => query = Some(p.text()?),
                1 => ci = p.boolean_value()?,
                2 => df = Some(p.text()?),
                3 => tz = Some(p.text()?),
                4 => boost = Some(p.float_value()?),
                _ => name = Some(p.text()?),
            }
            Ok(())
        })?;
        let Some(query) = query else { return Err(EsError::illegal_argument("Required [query]")) };
        let mut q = Kql { query, case_insensitive: ci, default_field: df, time_zone: tz, common: Common::default() };
        if let Some(b) = boost {
            q.common.boost = check_boost(b, || q.clone().into())?;
        }
        q.common.name = name;
        Ok(q)
    }

    pub fn inner(&self) -> Value {
        let mut m = Map::new();
        m.insert("query".into(), self.query.clone().into());
        if self.case_insensitive {
            m.insert("case_insensitive".into(), true.into());
        }
        if let Some(f) = &self.default_field {
            m.insert("default_field".into(), f.clone().into());
        }
        if let Some(z) = &self.time_zone {
            m.insert("time_zone".into(), z.clone().into());
        }
        self.common.write_if_set(&mut m);
        Value::Obj(m)
    }
}

/// A parsed KQL query.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    MatchAll,
    /// `field: *`.
    Exists(String),
    /// `field < value` and the other comparisons (`lt`, `lte`, `gt`, `gte`).
    Range { field: String, op: &'static str, value: String },
    /// `field: value`.
    Field { field: String, value: ValueExpr },
    /// A value without a field: every field.
    FieldLess(Text),
    /// `path: { query }`, field names inside relative to `path`.
    Nested { path: String, query: Box<Node> },
}

/// The value of a field query: one value, or a boolean expression of values in parentheses.
#[derive(Clone, Debug, PartialEq)]
pub enum ValueExpr {
    Leaf(Text),
    And(Vec<ValueExpr>),
    Or(Vec<ValueExpr>),
    Not(Box<ValueExpr>),
}

/// A value: its characters, each marked when it is an (unescaped) `*` wildcard.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Text {
    pub chars: Vec<(char, bool)>,
    pub quoted: bool,
}

impl Text {
    /// The value as written, escapes resolved.
    pub fn plain(&self) -> String {
        self.chars.iter().map(|c| c.0).collect()
    }

    pub fn has_wildcard(&self) -> bool {
        self.chars.iter().any(|c| c.1)
    }

    /// A `wildcard` query pattern: literal `*`, `?` and `\` escaped.
    pub fn wildcard_pattern(&self) -> String {
        let mut s = String::with_capacity(self.chars.len());
        for &(c, wild) in &self.chars {
            if !wild && matches!(c, '*' | '?' | '\\') {
                s.push('\\');
            }
            s.push(c);
        }
        s
    }

    /// A `query_string` query: Lucene's syntax escaped except the wildcards
    /// (`escapeLuceneQueryString(text, true)`).
    pub fn lucene_escaped(&self) -> String {
        let mut s = String::with_capacity(self.chars.len());
        for &(c, wild) in &self.chars {
            if !wild && "\\+-!():^[]\"{}~*?|&/".contains(c) {
                s.push('\\');
            }
            s.push(c);
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    And,
    Or,
    Not,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Colon,
    Op(&'static str),
    Quoted(String),
    Lit(Vec<(char, bool)>),
    Star,
    Eof,
}

#[derive(Clone, Debug)]
struct Lexed {
    tok: Tok,
    /// The token's text, for errors.
    text: String,
    /// Character offsets in the query.
    start: usize,
    end: usize,
}

fn err(col: usize, msg: impl std::fmt::Display) -> EsError {
    EsError::parsing(1, col, format!("line 1:{col}: {msg}"))
}

/// Characters that end an unquoted literal.
fn is_special(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\\' | '(' | ')' | ':' | '<' | '>' | '"' | '{' | '}')
}

fn unicode_escape(chars: &[char], i: usize) -> Option<char> {
    let hex: String = chars.get(i..i + 4)?.iter().collect();
    char::from_u32(u32::from_str_radix(&hex, 16).ok()?)
}

/// `\t`, `\n`, `\r`, `\uXXXX` or an escaped character at `chars[i]` (just past the `\`):
/// the character and how many characters the escape took.
fn escape(chars: &[char], i: usize) -> Option<(char, usize)> {
    match *chars.get(i)? {
        't' => Some(('\t', 1)),
        'n' => Some(('\n', 1)),
        'r' => Some(('\r', 1)),
        'u' => unicode_escape(chars, i + 1).map(|c| (c, 5)),
        c => Some((c, 1)),
    }
}

fn lex(src: &str) -> EsResult<Vec<Lexed>> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let tok = match c {
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            '{' => Tok::LBrace,
            '}' => Tok::RBrace,
            ':' => Tok::Colon,
            '<' | '>' => {
                let eq = chars.get(i + 1) == Some(&'=');
                if eq {
                    i += 1;
                }
                Tok::Op(match (c, eq) {
                    ('<', false) => "lt",
                    ('<', true) => "lte",
                    ('>', false) => "gt",
                    _ => "gte",
                })
            }
            '"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(err(start + 1, format!("token recognition error at: '{}'", chars[start..].iter().collect::<String>()))),
                        Some('"') => break,
                        Some('\\') => {
                            let Some((e, n)) = escape(&chars, i + 1) else {
                                return Err(err(i + 1, "token recognition error at: '\\'"));
                            };
                            s.push(e);
                            i += n;
                        }
                        Some(&ch) => s.push(ch),
                    }
                    i += 1;
                }
                Tok::Quoted(s)
            }
            _ => {
                let mut lit = Vec::new();
                let mut escaped = false;
                while i < chars.len() {
                    let ch = chars[i];
                    if ch == '\\' {
                        let Some((e, n)) = escape(&chars, i + 1) else {
                            return Err(err(i + 1, "token recognition error at: '\\'"));
                        };
                        lit.push((e, false));
                        escaped = true;
                        i += n + 1;
                        continue;
                    }
                    if is_special(ch) {
                        break;
                    }
                    lit.push((ch, ch == '*'));
                    i += 1;
                }
                i -= 1;
                let word: String = lit.iter().map(|c| c.0).collect();
                if lit.iter().all(|c| c.1) {
                    Tok::Star
                } else if !escaped && word.eq_ignore_ascii_case("and") {
                    Tok::And
                } else if !escaped && word.eq_ignore_ascii_case("or") {
                    Tok::Or
                } else if !escaped && word.eq_ignore_ascii_case("not") {
                    Tok::Not
                } else {
                    Tok::Lit(lit)
                }
            }
        };
        i += 1;
        out.push(Lexed { tok, text: chars[start..i].iter().collect(), start, end: i });
    }
    out.push(Lexed { tok: Tok::Eof, text: "<EOF>".into(), start: chars.len(), end: chars.len() });
    Ok(out)
}

/// Parses a KQL query: `None` for an empty one (match everything).
pub fn parse(src: &str) -> EsResult<Option<Node>> {
    let toks = lex(src)?;
    let mut p = Parser { src: src.chars().collect(), toks, pos: 0, nested: 0 };
    if p.peek() == &Tok::Eof {
        return Ok(None);
    }
    let n = p.or_query()?;
    if p.peek() != &Tok::Eof {
        return Err(p.unexpected("extraneous input", "<EOF>"));
    }
    Ok(Some(n))
}

struct Parser {
    src: Vec<char>,
    toks: Vec<Lexed>,
    pos: usize,
    /// Inside `path: { … }`, where values need a field.
    nested: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn bump(&mut self) -> Lexed {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn unexpected(&self, what: &str, expecting: &str) -> EsError {
        let t = &self.toks[self.pos];
        err(t.start + 1, format!("{what} '{}' expecting {expecting}", t.text))
    }

    fn expect(&mut self, tok: Tok, text: &str) -> EsResult<()> {
        if *self.peek() != tok {
            return Err(self.unexpected("mismatched input", &format!("'{text}'")));
        }
        self.bump();
        Ok(())
    }

    fn or_query(&mut self) -> EsResult<Node> {
        let mut xs = vec![self.and_query()?];
        while self.peek() == &Tok::Or {
            self.bump();
            xs.push(self.and_query()?);
        }
        Ok(if xs.len() == 1 { xs.pop().unwrap_or(Node::MatchAll) } else { Node::Or(xs) })
    }

    fn and_query(&mut self) -> EsResult<Node> {
        let mut xs = vec![self.simple()?];
        while self.peek() == &Tok::And {
            self.bump();
            match self.simple()? {
                Node::And(more) => xs.extend(more),
                n => xs.push(n),
            }
        }
        Ok(if xs.len() == 1 { xs.pop().unwrap_or(Node::MatchAll) } else { Node::And(xs) })
    }

    fn simple(&mut self) -> EsResult<Node> {
        match self.peek().clone() {
            Tok::Not => {
                self.bump();
                Ok(Node::Not(Box::new(self.simple()?)))
            }
            Tok::LParen => {
                self.bump();
                let n = self.or_query()?;
                self.expect(Tok::RParen, ")")?;
                Ok(n)
            }
            Tok::Star if self.peek_at(1) == &Tok::Colon && self.peek_at(2) == &Tok::Star && !self.value_follows(3) => {
                self.bump();
                self.bump();
                self.bump();
                Ok(Node::MatchAll)
            }
            Tok::Star if self.nested == 0 && !matches!(self.peek_at(1), Tok::Colon | Tok::Op(_)) && !self.value_follows(1) => {
                self.bump();
                Ok(Node::MatchAll)
            }
            Tok::Lit(_) | Tok::Quoted(_) | Tok::Star if matches!(self.peek_at(1), Tok::Colon | Tok::Op(_)) => self.field_query(),
            Tok::Lit(_) | Tok::Quoted(_) | Tok::Star if self.nested == 0 => Ok(Node::FieldLess(self.value()?)),
            _ => Err(self.unexpected("mismatched input", "{NOT, '(', QUOTED_STRING, UNQUOTED_LITERAL, WILDCARD}")),
        }
    }

    /// Whether the token `n` ahead continues a value (a literal or a wildcard).
    fn value_follows(&self, n: usize) -> bool {
        matches!(self.peek_at(n), Tok::Lit(_) | Tok::Star)
    }

    fn field_name(&mut self) -> String {
        match self.bump().tok {
            Tok::Lit(cs) => cs.iter().map(|c| c.0).collect(),
            Tok::Quoted(s) => s,
            _ => "*".into(),
        }
    }

    fn field_query(&mut self) -> EsResult<Node> {
        let field = self.field_name();
        if let Tok::Op(op) = self.bump().tok {
            let value = self.value()?.plain();
            return Ok(Node::Range { field, op, value });
        }
        match self.peek() {
            Tok::LBrace => {
                self.bump();
                self.nested += 1;
                let query = self.or_query()?;
                self.nested -= 1;
                self.expect(Tok::RBrace, "}")?;
                Ok(Node::Nested { path: field, query: Box::new(query) })
            }
            Tok::Star if !self.value_follows(1) => {
                self.bump();
                Ok(Node::Exists(field))
            }
            Tok::LParen => {
                self.bump();
                let value = self.value_or()?;
                self.expect(Tok::RParen, ")")?;
                Ok(Node::Field { field, value })
            }
            _ => Ok(Node::Field { field, value: ValueExpr::Leaf(self.value()?) }),
        }
    }

    fn value_or(&mut self) -> EsResult<ValueExpr> {
        let mut xs = vec![self.value_and()?];
        while self.peek() == &Tok::Or {
            self.bump();
            xs.push(self.value_and()?);
        }
        Ok(if xs.len() == 1 { xs.remove(0) } else { ValueExpr::Or(xs) })
    }

    fn value_and(&mut self) -> EsResult<ValueExpr> {
        let mut xs = vec![self.value_unary()?];
        while self.peek() == &Tok::And {
            self.bump();
            xs.push(self.value_unary()?);
        }
        Ok(if xs.len() == 1 { xs.remove(0) } else { ValueExpr::And(xs) })
    }

    fn value_unary(&mut self) -> EsResult<ValueExpr> {
        match self.peek() {
            Tok::Not => {
                self.bump();
                Ok(ValueExpr::Not(Box::new(self.value_unary()?)))
            }
            Tok::LParen => {
                self.bump();
                let v = self.value_or()?;
                self.expect(Tok::RParen, ")")?;
                Ok(v)
            }
            _ => Ok(ValueExpr::Leaf(self.value()?)),
        }
    }

    /// A quoted string, or unquoted literals and wildcards with the whitespace between them.
    fn value(&mut self) -> EsResult<Text> {
        if let Tok::Quoted(s) = self.peek() {
            let chars = s.chars().map(|c| (c, false)).collect();
            self.bump();
            return Ok(Text { chars, quoted: true });
        }
        if !self.value_follows(0) {
            return Err(self.unexpected("mismatched input", "{QUOTED_STRING, UNQUOTED_LITERAL, WILDCARD}"));
        }
        let mut text = Text::default();
        let mut prev_end = None;
        while self.value_follows(0) {
            let t = self.bump();
            if let Some(end) = prev_end {
                text.chars.extend(self.src[end..t.start].iter().map(|&c| (c, false)));
            }
            match t.tok {
                Tok::Lit(cs) => text.chars.extend(cs),
                _ => text.chars.extend(std::iter::repeat_n(('*', true), t.end - t.start)),
            }
            prev_end = Some(t.end);
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kql() {
        let leaf = |s: &str| ValueExpr::Leaf(Text { chars: s.chars().map(|c| (c, c == '*')).collect(), quoted: false });
        assert_eq!(parse("  ").unwrap(), None);
        assert_eq!(parse("*:*").unwrap(), Some(Node::MatchAll));
        assert_eq!(
            parse("a: foo  ba* AND NOT b >= 2 or c:* and d:{ e: (x OR \"y z\") }").unwrap(),
            Some(Node::Or(vec![
                Node::And(vec![
                    Node::Field { field: "a".into(), value: leaf("foo  ba*") },
                    Node::Not(Box::new(Node::Range { field: "b".into(), op: "gte", value: "2".into() })),
                ]),
                Node::And(vec![
                    Node::Exists("c".into()),
                    Node::Nested {
                        path: "d".into(),
                        query: Box::new(Node::Field {
                            field: "e".into(),
                            value: ValueExpr::Or(vec![leaf("x"), ValueExpr::Leaf(Text { chars: "y z".chars().map(|c| (c, false)).collect(), quoted: true })]),
                        }),
                    },
                ]),
            ]))
        );
        let Some(Node::FieldLess(t)) = parse(r"a\*b\:c*").unwrap() else { panic!() };
        assert_eq!((t.plain(), t.wildcard_pattern(), t.lucene_escaped()), ("a*b:c*".into(), r"a\*b:c*".into(), r"a\*b\:c*".into()));
        let e = parse("a: (b").unwrap_err();
        assert_eq!((&*e.kind, e.reason()), ("parsing_exception", "line 1:6: mismatched input '<EOF>' expecting ')'"));
    }
}
