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

fn parse_error(q: &str, msg: &str) -> EsError {
    EsError::new(400, "parse_exception", format!("Cannot parse '{q}': {msg}"))
}

/// Lucene's `QueryParserTokenManager`, roughly: the classic syntax as tokens.
fn tokenize(q: &str) -> EsResult<Vec<Tk>> {
    let chars: Vec<char> = q.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let special = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{3000}' | '+' | '-' | '!' | '(' | ')' | ':' | '^' | '[' | ']' | '"' | '{' | '}' | '~' | '\\' | '/');
    while i < chars.len() {
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
                i += 1;
                let s = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
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
                    return Err(parse_error(q, "Lexical error at line 1, column {}.  Encountered: <EOF> after : \"\\\"\"".replace("{}", &(chars.len() + 1).to_string()).as_str()));
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
                    return Err(parse_error(q, "Lexical error: unterminated regular expression"));
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
                        return Err(parse_error(q, "Lexical error: Term can not end with escape character."));
                    }
                    if special(ch) && !(ch == '-' && !s.is_empty()) && !(ch == '+' && !s.is_empty()) && !(ch == '!' && !s.is_empty()) {
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
                        // `>=x`, `<y`: Elasticsearch's range shortcuts.
                        if let Some(rest) = s.strip_prefix(">=") {
                            out.push(Tk::Range { lo: rest.to_string(), hi: "*".into(), il: true, iu: true });
                        } else if let Some(rest) = s.strip_prefix("<=") {
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
    Ok(out)
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

struct Parser<'c, 'q> {
    ctx: &'c QueryCtx<'q>,
    qs: &'c QueryString,
    toks: Vec<Tk>,
    pos: usize,
    src: String,
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
        let q = self.query(None)?;
        if self.pos < self.toks.len() {
            return Err(parse_error(&self.src, "Encountered \" \")\" \") \"\" at line 1"));
        }
        Ok(q)
    }

    fn query(&mut self, field: Option<&str>) -> EsResult<Option<QueryRef>> {
        let mut clauses: Vec<(Occur, QueryRef)> = Vec::new();
        let mut first: Option<QueryRef> = None;
        let mut count = 0;
        let mut first_conj = true;
        loop {
            match self.peek() {
                None | Some(Tk::RParen) => break,
                _ => {}
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
            let q = self.clause(field)?;
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
        if let Some(Tk::Caret(b)) = self.peek().cloned() {
            self.pos += 1;
            let v: f32 = b.parse().map_err(|_| parse_error(&self.src, "Encountered \"^\""))?;
            return Ok(q.map(|q| if v == 1.0 { q } else { arc(BoostQuery::new(q, v)) as QueryRef }));
        }
        Ok(q)
    }

    fn clause(&mut self, field: Option<&str>) -> EsResult<Option<QueryRef>> {
        let mut field: Option<String> = field.map(str::to_string);
        if let Some(Tk::Field(f)) = self.peek().cloned() {
            self.pos += 1;
            field = Some(f);
        }
        match self.next() {
            Some(Tk::LParen) => {
                let q = self.query(field.as_deref())?;
                match self.next() {
                    Some(Tk::RParen) => {}
                    _ => return Err(parse_error(&self.src, "Encountered \"<EOF>\" at line 1. Was expecting \")\"")),
                }
                self.boost(q)
            }
            Some(Tk::Term(t, wild)) => {
                let mut fuzzy = None;
                if let Some(Tk::Tilde(n)) = self.peek().cloned() {
                    self.pos += 1;
                    fuzzy = Some(n);
                }
                let q = self.term(field.as_deref(), &t, wild, fuzzy)?;
                self.boost(q)
            }
            Some(Tk::Phrase(p)) => {
                let mut slop = self.qs.phrase_slop;
                if let Some(Tk::Tilde(n)) = self.peek().cloned() {
                    self.pos += 1;
                    slop = n.and_then(|s| s.parse::<f32>().ok()).map(|f| f as i32).unwrap_or(slop);
                }
                let q = self.phrase(field.as_deref(), &p, slop)?;
                self.boost(q)
            }
            Some(Tk::Regex(r)) => {
                // The length limit is not lenient.
                self.ctx.check_regex_length(&r, "Regexp")?;
                let q = self.each_field(field.as_deref(), &mut |ctx, f| {
                    let rq = crate::query::term_level::Regexp {
                        field: f.to_string(),
                        value: r.clone(),
                        flags: crate::query::term_level::regexp_flags::LUCENE_ALL,
                        max_determinized_states: 10000,
                        rewrite: None,
                        case_insensitive: false,
                        common: Default::default(),
                    };
                    ctx.to_query(&crate::query::Query::Regexp(Box::new(rq))).map(Some)
                })?;
                self.boost(q)
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
            Some(other) => Err(parse_error(&self.src, &format!("Encountered \"{}\" at line 1", tk_text(&other)))),
            None => Err(parse_error(&self.src, "Encountered \"<EOF>\" at line 1")),
        }
    }

    /// The fields a clause runs on (its own, or the default ones), with boosts.
    fn fields_of(&self, field: Option<&str>) -> Vec<(String, f32)> {
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
        // All-fields mode (no `fields`, the default field `*`) is lenient by default, for every
        // clause, its own fields included.
        let _ = field;
        self.qs.lenient.unwrap_or_else(|| {
            self.qs.fields.is_empty() && self.qs.default_field.as_deref().is_none_or(|d| d == "*") && text::default_fields(self.ctx).iter().any(|(f, _)| f == "*")
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
            if field.is_none() || field == Some("*") {
                return Ok(Some(arc(search_index::query::MatchAllQuery)));
            }
            let f = field.unwrap();
            return self.ctx.exists_query(f).map(Some);
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
        let tie = self.qs.tie_breaker.unwrap_or(if self.qs.ty == MultiMatchType::MostFields { 1.0 } else { 0.0 });
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

fn tk_text(t: &Tk) -> String {
    match t {
        Tk::RParen => ")".into(),
        Tk::LParen => "(".into(),
        Tk::And => "AND".into(),
        Tk::Or => "OR".into(),
        Tk::Caret(_) => "^".into(),
        Tk::Tilde(_) => "~".into(),
        Tk::Plus => "+".into(),
        Tk::Minus => "-".into(),
        Tk::Not => "NOT".into(),
        Tk::Field(f) => format!("{f}:"),
        Tk::Term(t, _) => t.clone(),
        Tk::Phrase(p) => format!("\"{p}\""),
        Tk::Regex(r) => format!("/{r}/"),
        Tk::Range { .. } => "[".into(),
    }
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
    let toks = tokenize(&qs.query).map_err(wrap)?;
    let mut p = Parser { ctx, qs, toks, pos: 0, src: qs.query.clone() };
    let q = p.top().map_err(wrap)?;
    let q = match q {
        Some(q) => q,
        None => return Ok(match_none("Matching no documents because no terms present")),
    };
    text::apply_msm(q, qs.minimum_should_match.as_deref())
}

/// `simple_query_string`: `+` (and), `|` (or), `-` (not), `"phrase"`, `prefix*`, `(…)`, `~N`;
/// whitespace is the default operator. Never fails on syntax.
pub fn simple_query_string(ctx: &QueryCtx, s: &SimpleQueryString) -> EsResult<QueryRef> {
    let explicit = !s.fields.is_empty();
    let fields = if explicit { text::resolve_fields(ctx, &s.fields) } else { text::resolve_fields(ctx, &text::default_fields(ctx)) };
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
        let mut slop = None;
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
                        if st.index + 1 < st.end {
                            slop = Some(self.parse_fuzziness(st));
                        }
                    }
                    break;
                }
            }
            escaped = false;
            text.push(c);
            st.index += 1;
        }
        if st.index == st.end {
            // No closing quote: the opening one is ignored.
            st.index = start;
        } else if text.is_empty() {
            st.current = None;
            st.index += 1;
        } else {
            let q = self.text_query(&text, true, slop.unwrap_or(0))?;
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
        Ok(match qs.len() {
            0 => None,
            1 => qs.pop(),
            _ => {
                let mut b = BooleanQuery::new();
                for q in qs {
                    b = b.add(Occur::Should, q);
                }
                Some(arc(b))
            }
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
