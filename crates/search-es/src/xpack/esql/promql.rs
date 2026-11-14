//! `PROMQL`: a PromQL query as a source command, Elasticsearch's translation of it into a `TS`
//! query.
//!
//! `PROMQL index=<index> step=<duration> [name=](<expression>)` evaluates the expression at
//! the end of every `step`-long bucket: `TS <index> | STATS name = agg(fn(metric)) BY step =
//! TBUCKET(step), labels`, the step stamped at the bucket's end, values as doubles and steps
//! without a value dropped. A metric or label no index maps reads as null (PromQL's empty
//! result), as `SET unmapped_fields="nullify"` does.
//!
//! Supported: an aggregation (`sum`, `avg`, `min`, `max`, `count`, optionally `by (labels)`)
//! over a range function (`rate`, `irate`, `increase`, `delta`, `idelta`, `deriv`, the
//! `*_over_time` functions) of a selector whose range is the step, or over an instant selector
//! (each series' last value in the step); selectors take label matchers (`=`, `!=`, `=~`,
//! `!~`) with constants or parameters (`?name`, `?1`; a multi-valued one matches any of its
//! values). Not yet: ranges other than the step, binary operators, `without`, `offset`/`@`,
//! `start`/`end`/`buckets` and expressions that aren't aggregated (their `_timeseries`
//! column).

use super::parser::{BinOp, Cmd, Expr, ExprKind, Named, Param, parse_error};
use super::value::V;
use crate::error::{EsError, EsResult};

/// The byte range of a `PROMQL` source command in `src` (after any `SET` statements, up to
/// the first `|` outside brackets and quotes), if the query has one.
pub fn find(src: &str) -> Option<(usize, usize)> {
    let b = src.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let word = |i: usize, w: &str| src.get(i..i + w.len()).is_some_and(|s| s.eq_ignore_ascii_case(w)) && b.get(i + w.len()).is_none_or(|c| c.is_ascii_whitespace());
    let mut i = skip_ws(0);
    while word(i, "SET") {
        let mut quote = None;
        while i < b.len() && (quote.is_some() || b[i] != b';') {
            match (quote, b[i]) {
                (None, q @ (b'"' | b'\'')) => quote = Some(q),
                (Some(q), c) if c == q => quote = None,
                _ => {}
            }
            i += 1;
        }
        i = skip_ws(i + 1);
    }
    if !word(i, "PROMQL") {
        return None;
    }
    let start = i;
    let (mut depth, mut quote) = (0i32, None);
    while i < b.len() {
        match (quote, b[i]) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), b'\\') => i += 1,
            (Some(_), _) => {}
            (None, q @ (b'"' | b'\'' | b'`')) => quote = Some(q),
            (None, b'(' | b'[' | b'{') => depth += 1,
            (None, b')' | b']' | b'}') => depth -= 1,
            (None, b'|') if depth == 0 => break,
            _ => {}
        }
        i += 1;
    }
    Some((start, i))
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    /// A number with a unit (`1m`, `30s`) or without.
    Num(String),
    Param(String),
    Op(&'static str),
    Eof,
}

struct Lexed {
    tok: Tok,
    start: usize,
    end: usize,
}

fn lex(src: &str, from: usize, to: usize) -> EsResult<Vec<Lexed>> {
    let b = src.as_bytes();
    let mut out: Vec<Lexed> = Vec::new();
    let mut i = from;
    let ident = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b':' | b'-' | b'*');
    while i < to {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let tok = match c {
            b'"' | b'\'' | b'`' => {
                let mut s = String::new();
                i += 1;
                while i < to && b[i] != c {
                    if b[i] == b'\\' && i + 1 < to {
                        i += 1;
                    }
                    let ch = src[i..].chars().next().expect("in bounds");
                    s.push(ch);
                    i += ch.len_utf8();
                }
                if i >= to {
                    return Err(parse_error(src, start, "unterminated quoted string"));
                }
                i += 1;
                Tok::Str(s)
            }
            b'?' => {
                i += 1;
                while i < to && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                Tok::Param(src[start + 1..i].to_string())
            }
            b'0'..=b'9' => {
                while i < to && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                Tok::Num(src[start..i].to_string())
            }
            b'!' | b'=' => {
                let op = match src.get(i..i + 2).unwrap_or("") {
                    "!=" => "!=",
                    "=~" => "=~",
                    "!~" => "!~",
                    _ if c == b'=' => "=",
                    _ => return Err(parse_error(src, start, "token recognition error at: '!'")),
                };
                i += op.len();
                Tok::Op(op)
            }
            b'(' | b')' | b'{' | b'}' | b'[' | b']' | b',' => {
                i += 1;
                Tok::Op(match c {
                    b'(' => "(",
                    b')' => ")",
                    b'{' => "{",
                    b'}' => "}",
                    b'[' => "[",
                    b']' => "]",
                    _ => ",",
                })
            }
            _ if ident(c) => {
                while i < to && ident(b[i]) {
                    i += 1;
                }
                Tok::Ident(src[start..i].to_string())
            }
            _ => {
                let len = src[start..].chars().next().map_or(1, char::len_utf8);
                return Err(parse_error(src, start, &format!("token recognition error at: '{}'", &src[start..start + len])));
            }
        };
        out.push(Lexed { tok, start, end: i });
    }
    out.push(Lexed { tok: Tok::Eof, start: to, end: to });
    Ok(out)
}

/// A PromQL expression.
enum Node {
    Agg { op: String, by: Vec<(String, usize)>, inner: Box<Node>, at: usize, end: usize },
    Func { name: String, arg: Box<Node>, at: usize, end: usize },
    Select { metric: String, matchers: Vec<Matcher>, range: Option<(String, usize)>, at: usize, end: usize },
}

impl Node {
    fn span(&self) -> (usize, usize) {
        match self {
            Node::Agg { at, end, .. } | Node::Func { at, end, .. } | Node::Select { at, end, .. } => (*at, *end),
        }
    }
}

struct Matcher {
    label: String,
    op: &'static str,
    value: V,
    at: usize,
    end: usize,
}

const AGGREGATIONS: &[&str] = &["sum", "avg", "min", "max", "count"];

/// PromQL's range functions and the `TS` function each is.
fn range_function(name: &str) -> Option<&'static str> {
    Some(match name {
        "rate" => "rate",
        "irate" => "irate",
        "increase" => "increase",
        "delta" => "delta",
        "idelta" => "idelta",
        "deriv" => "deriv",
        "avg_over_time" => "avg_over_time",
        "sum_over_time" => "sum_over_time",
        "min_over_time" => "min_over_time",
        "max_over_time" => "max_over_time",
        "count_over_time" => "count_over_time",
        "last_over_time" => "last_over_time",
        "present_over_time" => "present_over_time",
        "stddev_over_time" => "stddev_over_time",
        "stdvar_over_time" => "variance_over_time",
        _ => return None,
    })
}

/// A PromQL duration (`30s`, `1m`, `2h`) as an ES|QL span literal's text.
fn span_of(d: &str) -> Option<String> {
    let split = d.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = d.split_at(split);
    let n: i64 = n.parse().ok()?;
    let word = match unit {
        "ms" => "millisecond",
        "s" => "second",
        "m" => "minute",
        "h" => "hour",
        "d" => "day",
        "w" => "week",
        "y" => "year",
        _ => return None,
    };
    Some(format!("{n} {word}"))
}

fn location_msg(src: &str, at: usize, msg: &str) -> String {
    let (l, c) = super::parser::location(src, at);
    format!("line {l}:{c}: {msg}")
}

fn verification(src: &str, at: usize, msg: &str) -> EsError {
    EsError::new(400, "verification_exception", format!("Found 1 problem\n{}", location_msg(src, at, msg)))
}

struct P<'a> {
    src: &'a str,
    toks: Vec<Lexed>,
    i: usize,
    params: &'a [(Option<String>, Param)],
}

impl P<'_> {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }

    fn at(&self) -> usize {
        self.toks[self.i].start
    }

    fn bump(&mut self) -> usize {
        let end = self.toks[self.i].end;
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        end
    }

    fn expect(&mut self, op: &'static str) -> EsResult<usize> {
        if *self.peek() == Tok::Op(op) {
            return Ok(self.bump());
        }
        let at = self.at();
        let found = &self.src[at..self.toks[self.i].end];
        Err(parse_error(self.src, at, &format!("mismatched input '{}' expecting '{op}'", if found.is_empty() { "<EOF>" } else { found })))
    }

    fn param(&self, name: &str, at: usize) -> EsResult<V> {
        let found = match name.parse::<usize>() {
            Ok(n) => n.checked_sub(1).and_then(|i| self.params.get(i)),
            Err(_) => self.params.iter().find(|(k, _)| k.as_deref() == Some(name)),
        };
        match found {
            Some((_, Param::Value(v))) => Ok(v.clone()),
            Some((_, Param::Identifier(s) | Param::Pattern(s))) => Ok(V::Str(s.clone())),
            None => Err(parse_error(self.src, at, &format!("Parameter [?{name}] value not found"))),
        }
    }

    /// `by (labels)` when it comes next.
    fn grouping(&mut self) -> EsResult<Vec<(String, usize)>> {
        let Tok::Ident(w) = self.peek().clone() else { return Ok(Vec::new()) };
        if w.eq_ignore_ascii_case("without") {
            return Err(verification(self.src, self.at(), "PromQL [without] grouping is not supported"));
        }
        if !w.eq_ignore_ascii_case("by") {
            return Ok(Vec::new());
        }
        self.bump();
        self.expect("(")?;
        let mut out = Vec::new();
        while let Tok::Ident(l) = self.peek().clone() {
            out.push((l, self.at()));
            self.bump();
            if *self.peek() == Tok::Op(",") {
                self.bump();
            }
        }
        self.expect(")")?;
        Ok(out)
    }

    fn expr(&mut self) -> EsResult<Node> {
        let at = self.at();
        let Tok::Ident(name) = self.peek().clone() else {
            let found = &self.src[at..self.toks[self.i].end];
            return Err(parse_error(self.src, at, &format!("mismatched input '{}' expecting an expression", if found.is_empty() { "<EOF>" } else { found })));
        };
        self.bump();
        let lower = name.to_ascii_lowercase();
        let grouped = matches!(self.peek(), Tok::Ident(w) if w.eq_ignore_ascii_case("by") || w.eq_ignore_ascii_case("without"));
        if grouped || *self.peek() == Tok::Op("(") {
            let known = AGGREGATIONS.contains(&lower.as_str()) || (!grouped && range_function(&lower).is_some());
            if !known {
                return Err(verification(self.src, at, &format!("Unknown PromQL function [{name}]")));
            }
            if AGGREGATIONS.contains(&lower.as_str()) {
                let mut by = self.grouping()?;
                self.expect("(")?;
                let inner = self.expr()?;
                let mut end = self.expect(")")?;
                if by.is_empty() {
                    by = self.grouping()?;
                    if !by.is_empty() {
                        end = self.toks[self.i - 1].end;
                    }
                }
                return Ok(Node::Agg { op: lower, by, inner: Box::new(inner), at, end });
            }
            self.bump();
            let arg = self.expr()?;
            let end = self.expect(")")?;
            return Ok(Node::Func { name: lower, arg: Box::new(arg), at, end });
        }
        let mut matchers = Vec::new();
        let mut end = self.toks[self.i - 1].end;
        if *self.peek() == Tok::Op("{") {
            self.bump();
            while let Tok::Ident(label) = self.peek().clone() {
                let m_at = self.at();
                self.bump();
                let op = match self.peek() {
                    Tok::Op(o @ ("=" | "!=" | "=~" | "!~")) => *o,
                    _ => return Err(parse_error(self.src, self.at(), "mismatched input expecting {'=', '!=', '=~', '!~'}")),
                };
                self.bump();
                let v_at = self.at();
                let value = match self.peek().clone() {
                    Tok::Str(s) => V::Str(s),
                    Tok::Param(p) => self.param(&p, v_at)?,
                    _ => return Err(parse_error(self.src, v_at, "mismatched input expecting a string")),
                };
                let m_end = self.bump();
                matchers.push(Matcher { label, op, value, at: m_at, end: m_end });
                if *self.peek() == Tok::Op(",") {
                    self.bump();
                }
            }
            end = self.expect("}")?;
        }
        let mut range = None;
        if *self.peek() == Tok::Op("[") {
            self.bump();
            let r_at = self.at();
            let Tok::Num(d) = self.peek().clone() else { return Err(parse_error(self.src, r_at, "mismatched input expecting a duration")) };
            self.bump();
            range = Some((d, r_at));
            end = self.expect("]")?;
        }
        Ok(Node::Select { metric: name, matchers, range, at, end })
    }
}

/// The commands `src[start..end]` (a `PROMQL` command) stands for.
pub fn desugar(src: &str, start: usize, end: usize, params: &[(Option<String>, Param)]) -> EsResult<Vec<Cmd>> {
    let toks = lex(src, start + "PROMQL".len(), end)?;
    let mut p = P { src, toks, i: 0, params };
    let (mut index, mut step) = (None, None);
    let mut name = None;
    // Settings (`index=test`), then `name=(expression)` or the expression.
    while let Tok::Ident(k) = p.peek().clone() {
        let k_at = p.at();
        if p.toks.get(p.i + 1).map(|t| &t.tok) != Some(&Tok::Op("=")) {
            break;
        }
        if p.toks.get(p.i + 2).map(|t| &t.tok) == Some(&Tok::Op("(")) {
            p.bump();
            p.bump();
            p.bump();
            name = Some(k);
            break;
        }
        p.bump();
        p.bump();
        let mut v = match p.peek().clone() {
            Tok::Ident(s) | Tok::Num(s) | Tok::Str(s) => s,
            Tok::Param(n) => p.param(&n, p.at())?.to_text(),
            _ => return Err(parse_error(src, p.at(), &format!("mismatched input expecting a value for [{k}]"))),
        };
        p.bump();
        // Index lists: `index=a,b`.
        while k.eq_ignore_ascii_case("index") && *p.peek() == Tok::Op(",") {
            p.bump();
            if let Tok::Ident(s) = p.peek().clone() {
                v = format!("{v},{s}");
                p.bump();
            }
        }
        match k.to_ascii_lowercase().as_str() {
            "index" => index = Some(v),
            "step" => step = Some((v, k_at)),
            "start" | "end" | "buckets" => return Err(verification(src, k_at, &format!("PROMQL [{k}] is not supported"))),
            _ => return Err(parse_error(src, k_at, &format!("Unknown PROMQL parameter [{k}]"))),
        }
    }
    let node = p.expr()?;
    if name.is_some() {
        p.expect(")")?;
    }
    if *p.peek() != Tok::Eof {
        let at = p.at();
        return Err(parse_error(src, at, &format!("extraneous input '{}' expecting <EOF>", &src[at..p.toks[p.i].end])));
    }
    let name = name.unwrap_or_else(|| {
        let (s, e) = node.span();
        src[s..e].to_string()
    });
    let Some(index) = index else { return Err(verification(src, start, "PROMQL requires an [index]")) };
    let Some((step, step_at)) = step else {
        return Err(verification(src, start, &format!("unable to create a bucket; provide either [step] or all of [start], [end], and [buckets] [{}]", src[start..end].trim_end())));
    };
    let Some(step_span) = span_of(&step) else { return Err(parse_error(src, step_at, &format!("invalid duration [{step}]"))) };

    let Node::Agg { op, by, inner, at: agg_at, end: agg_end } = node else {
        return Err(verification(src, node.span().0, "PROMQL without an aggregation is not supported"));
    };
    let expr = |kind: ExprKind, at: usize, end: usize| Expr { kind, start: at, end };
    let col = |n: &str, at: usize, end: usize| expr(ExprKind::Col(n.to_string()), at, end);
    let (call, matchers) = match *inner {
        Node::Func { name: f, arg, at, end } => {
            let Node::Select { metric, matchers, range: Some((r, r_at)), at: s_at, end: s_end } = *arg else {
                return Err(verification(src, at, &format!("[{f}] needs a range vector")));
            };
            if span_of(&r) != Some(step_span.clone()) {
                return Err(verification(src, r_at, &format!("a range [{r}] other than the step [{step}] is not supported")));
            }
            let fname = range_function(&f).expect("parsed");
            (expr(ExprKind::Call(fname.into(), vec![col(&metric, s_at, s_end)]), at, end), matchers)
        }
        // An instant vector: each series' last value in the step.
        Node::Select { metric, matchers, range: None, at, end } => (expr(ExprKind::Call("last_over_time".into(), vec![col(&metric, at, end)]), at, end), matchers),
        other => return Err(verification(src, other.span().0, "this PROMQL expression is not supported")),
    };

    let mut cmds = vec![Cmd::Ts { sources: index.split(',').map(str::to_string).collect(), metadata: Vec::new(), at: start }];
    let mut cond: Option<Expr> = None;
    for m in matchers {
        let c = col(&m.label, m.at, m.end);
        let values: Vec<V> = match &m.value {
            V::Multi(vs) => vs.clone(),
            v => vec![v.clone()],
        };
        let regex = |escape: bool| {
            let alts: Vec<String> = values.iter().map(|v| if escape { regex_escape(&v.to_text()) } else { v.to_text() }).collect();
            if alts.len() == 1 { alts[0].clone() } else { format!("({})", alts.join("|")) }
        };
        let e = match (m.op, values.as_slice()) {
            ("=", [v]) => expr(ExprKind::Bin(BinOp::Eq, Box::new(c), Box::new(expr(ExprKind::Lit(v.clone()), m.at, m.end))), m.at, m.end),
            ("!=", [v]) => expr(ExprKind::Bin(BinOp::Neq, Box::new(c), Box::new(expr(ExprKind::Lit(v.clone()), m.at, m.end))), m.at, m.end),
            // Several values match any of them (an exact matcher becomes a regex).
            ("=" | "=~", _) => expr(ExprKind::Like(Box::new(c), regex(m.op == "="), true, false), m.at, m.end),
            _ => expr(ExprKind::Like(Box::new(c), regex(m.op == "!="), true, true), m.at, m.end),
        };
        cond = Some(match cond {
            None => e,
            Some(prev) => {
                let (s, en) = (prev.start, e.end);
                expr(ExprKind::Bin(BinOp::And, Box::new(prev), Box::new(e)), s, en)
            }
        });
    }
    if let Some(c) = cond {
        cmds.push(Cmd::Where(c));
    }
    let span = |at: usize| expr(ExprKind::Span(step_span.clone()), at, at);
    let mut group = vec![Named { name: "step".into(), expr: expr(ExprKind::Call("tbucket".into(), vec![span(step_at)]), step_at, step_at) }];
    group.extend(by.iter().map(|(l, at)| Named { name: l.clone(), expr: col(l, *at, at + l.len()) }));
    cmds.push(Cmd::Stats { aggs: vec![Named { name: name.clone(), expr: expr(ExprKind::Call(op, vec![call]), agg_at, agg_end) }], by: group });
    // Each step is reported at its bucket's end.
    let shifted = expr(ExprKind::Bin(BinOp::Add, Box::new(col("step", step_at, step_at)), Box::new(span(step_at))), step_at, step_at);
    let value = expr(ExprKind::Call("to_double".into(), vec![col(&name, agg_at, agg_end)]), agg_at, agg_end);
    cmds.push(Cmd::Eval(vec![Named { name: "step".into(), expr: shifted }, Named { name: name.clone(), expr: value }]));
    cmds.push(Cmd::Where(expr(ExprKind::IsNull(Box::new(col(&name, agg_at, agg_end)), true), agg_at, agg_end)));
    let mut keep = vec![(name, agg_at), ("step".to_string(), step_at)];
    keep.extend(by);
    cmds.push(Cmd::Keep(keep));
    Ok(cmds)
}

/// How many of `cmds` (a query whose source is a `PROMQL` command) [`desugar`] made: `TS`,
/// the matchers' `WHERE` if any, `STATS`, `EVAL`, `WHERE`, `KEEP`.
pub fn desugared_len(cmds: &[Cmd]) -> usize {
    if matches!(cmds.get(1), Some(Cmd::Where(_))) { 6 } else { 5 }
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$|?*+()[]{}#~\"&<>@".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_command_after_settings() {
        let q = r#"SET unmapped_fields="nullify"; PROMQL index=test step=1m x=(sum(rate(a[1m]))) | SORT step"#;
        let (s, e) = find(q).unwrap();
        assert_eq!(&q[s..e], "PROMQL index=test step=1m x=(sum(rate(a[1m]))) ");
        assert!(find("FROM test | EVAL x = 1").is_none());
    }

    #[test]
    fn desugars_to_a_time_series_query() {
        let q = "PROMQL index=test step=1m x=(sum by (k8s.pod.name) (rate(k8s.pod.network.tx{k8s.pod.name=\"cat\"}[1m])))";
        let cmds = desugar(q, 0, q.len(), &[]).unwrap();
        assert!(matches!(&cmds[0], Cmd::Ts { sources, .. } if sources == &["test"]));
        assert!(matches!(&cmds[1], Cmd::Where(_)));
        assert!(matches!(&cmds[2], Cmd::Stats { by, .. } if by.len() == 2 && by[1].name == "k8s.pod.name"));
        let q = "PROMQL index=test step=1m x=(non_existent_function(k8s.pod.network.tx))";
        assert_eq!(desugar(q, 0, q.len(), &[]).unwrap_err().reason(), "Found 1 problem\nline 1:30: Unknown PromQL function [non_existent_function]");
    }
}
