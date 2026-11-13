//! SQL (`POST _sql`, `_sql/translate`, cursors, async queries), compiled to the ES|QL engine.
//!
//! A `SELECT` becomes an ES|QL pipeline: `FROM` the table, `WHERE`, `STATS ... BY` for
//! `GROUP BY` / aggregates (with `HAVING` as a `WHERE` over the groups), `EVAL` for computed
//! columns, `KEEP` in select order, `SORT`, `LIMIT`. Results print with SQL's type names
//! (`datetime`, `float` kept as `float`) and paginate with `fetch_size` through cursors kept
//! in memory. `SHOW TABLES`, `SHOW COLUMNS`/`DESCRIBE` and `SHOW FUNCTIONS` are supported.

use super::esql::exec::{self, Table};
use super::esql::parser::{BinOp, Cmd, Expr, ExprKind, Named, Query, SortKey};
use super::esql::value::{DataType, V, Warnings};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::async_results::{self, Entry, Kind};
use crate::xpack::internal;

fn parsing(src: &str, at: usize, msg: &str) -> EsError {
    let (l, c) = super::esql::parser::location(src, at);
    EsError::new(400, "parsing_exception", format!("line {l}:{c}: {msg}"))
}

#[derive(Clone, Debug, PartialEq)]
enum T {
    Word(String),
    Quoted(String),
    Str(String),
    Int(i64),
    Dec(f64),
    Op(&'static str),
    Eof,
}

const OPS: &[&str] = &["<>", "!=", "<=", ">=", "::", "=", "<", ">", "+", "-", "*", "/", "%", "(", ")", ",", ".", "?"];

fn lex(src: &str) -> EsResult<Vec<(T, usize, usize)>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        if c == b'\'' || c == b'"' {
            let q = c;
            let mut s = String::new();
            i += 1;
            loop {
                let Some(&ch) = b.get(i) else { return Err(parsing(src, start, "token recognition error")) };
                if ch == q {
                    if b.get(i + 1) == Some(&q) {
                        s.push(q as char);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                let n = src[i..].chars().next().map_or(1, char::len_utf8);
                s.push_str(&src[i..i + n]);
                i += n;
            }
            out.push((if q == b'\'' { T::Str(s) } else { T::Quoted(s) }, start, i));
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let mut dec = false;
            if j < b.len() && b[j] == b'.' {
                dec = true;
                j += 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
            }
            if j < b.len() && (b[j] == b'e' || b[j] == b'E') {
                dec = true;
                j += 1;
                if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                    j += 1;
                }
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
            }
            let t = &src[i..j];
            out.push((if dec { T::Dec(t.parse().unwrap_or(0.0)) } else { t.parse::<i64>().map(T::Int).unwrap_or(T::Dec(t.parse().unwrap_or(0.0))) }, start, j));
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c == b'@' || c >= 0x80 {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'@' || b[j] >= 0x80) {
                j += 1;
            }
            out.push((T::Word(src[i..j].to_string()), start, j));
            i = j;
            continue;
        }
        let Some(op) = OPS.iter().find(|o| src[i..].starts_with(*o)) else {
            return Err(parsing(src, start, &format!("token recognition error at: '{}'", c as char)));
        };
        out.push((T::Op(op), start, i + op.len()));
        i += op.len();
    }
    out.push((T::Eof, src.len(), src.len()));
    Ok(out)
}

struct P<'a> {
    src: &'a str,
    t: Vec<(T, usize, usize)>,
    i: usize,
    /// `params` for the `?` placeholders, in order.
    params: &'a [V],
    next_param: usize,
}

/// What may start a select item (ANTLR's expected set after `SELECT`).
const SELECT_ITEM_START: &str = "'ALL', 'ANALYZE', 'ANALYZED', 'CASE', 'CAST', 'CATALOGS', 'COLUMNS', 'CONVERT', 'CURRENT_DATE', 'CURRENT_TIME', 'CURRENT_TIMESTAMP', 'DAY', 'DEBUG', 'DISTINCT', 'EXECUTABLE', 'EXISTS', 'EXPLAIN', 'EXTRACT', 'FALSE', 'FIRST', 'FORMAT', 'FULL', 'FUNCTIONS', 'GRAPHVIZ', 'HOUR', 'INTERVAL', 'LAST', 'LEFT', 'LIMIT', 'MAPPED', 'MATCH', 'MINUTE', 'MONTH', 'NOT', 'NULL', 'OPTIMIZED', 'PARSED', 'PHYSICAL', 'PIVOT', 'PLAN', 'RIGHT', 'RLIKE', 'QUERY', 'SCHEMAS', 'SECOND', 'SHOW', 'SYS', 'TABLES', 'TEXT', 'TRUE', 'TOP', 'TYPE', 'TYPES', 'VERIFY', 'YEAR', FUNCTION_ESC, DATE_ESC, TIME_ESC, TIMESTAMP_ESC, GUID_ESC, '(', '+', '-', '*', '?', STRING, INTEGER_VALUE, DECIMAL_VALUE, IDENTIFIER, DIGIT_IDENTIFIER, QUOTED_IDENTIFIER, BACKQUOTED_IDENTIFIER";

const RESERVED: &[&str] = &["FROM", "WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "AS", "AND", "OR", "NOT", "BY", "ASC", "DESC", "IS", "NULL", "LIKE", "RLIKE", "IN", "BETWEEN", "WHEN", "THEN", "ELSE", "END", "NULLS", "FETCH", "OFFSET"];

impl P<'_> {
    fn peek(&self) -> &T {
        &self.t[self.i].0
    }
    fn pos(&self) -> usize {
        self.t[self.i].1
    }
    fn bump(&mut self) -> (T, usize, usize) {
        let r = self.t[self.i].clone();
        if self.i + 1 < self.t.len() {
            self.i += 1;
        }
        r
    }
    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), T::Word(w) if w.eq_ignore_ascii_case(k))
    }
    fn eat_kw(&mut self, k: &str) -> bool {
        if self.is_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn eat_op(&mut self, o: &str) -> bool {
        if matches!(self.peek(), T::Op(x) if *x == o) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn text(&self) -> String {
        let (t, s, e) = &self.t[self.i];
        if *t == T::Eof { "<EOF>".into() } else { self.src[*s..*e].to_string() }
    }
    fn err(&self, expecting: &str) -> EsError {
        parsing(self.src, self.pos(), &format!("mismatched input '{}' expecting {expecting}", self.text()))
    }
    fn expect_kw(&mut self, k: &str) -> EsResult<()> {
        if self.eat_kw(k) { Ok(()) } else { Err(self.err(&format!("'{k}'"))) }
    }

    /// A (possibly dotted or quoted) identifier.
    fn ident(&mut self) -> EsResult<String> {
        let mut out = match self.bump() {
            (T::Word(w), ..) => w,
            (T::Quoted(q), ..) => q,
            _ => {
                self.i -= 1;
                return Err(self.err("{QUOTED_IDENTIFIER, IDENTIFIER}"));
            }
        };
        while matches!(self.peek(), T::Op(".")) {
            self.bump();
            match self.bump() {
                (T::Word(w), ..) | (T::Quoted(w), ..) => {
                    out.push('.');
                    out.push_str(&w);
                }
                (T::Op("*"), ..) => out.push_str(".*"),
                _ => return Err(self.err("IDENTIFIER")),
            }
        }
        Ok(out)
    }

    /// A table name: an identifier, possibly with `-` and `*` (unquoted index patterns).
    fn table(&mut self) -> EsResult<String> {
        if let T::Quoted(q) = self.peek().clone() {
            self.bump();
            return Ok(q);
        }
        let start = self.pos();
        let mut end = start;
        loop {
            match self.peek() {
                T::Word(_) | T::Int(_) | T::Op("-") | T::Op("*") | T::Op(".") if self.pos() == end || end == start => {
                    let (_, _, e) = self.bump();
                    end = e;
                }
                _ => break,
            }
        }
        if end == start {
            return Err(self.err("{QUOTED_IDENTIFIER, IDENTIFIER}"));
        }
        Ok(self.src[start..end].to_string())
    }

    fn expr(&mut self) -> EsResult<Expr> {
        self.or()
    }
    fn or(&mut self) -> EsResult<Expr> {
        let mut l = self.and()?;
        while self.eat_kw("OR") {
            let r = self.and()?;
            l = bin(BinOp::Or, l, r);
        }
        Ok(l)
    }
    fn and(&mut self) -> EsResult<Expr> {
        let mut l = self.not()?;
        while self.eat_kw("AND") {
            let r = self.not()?;
            l = bin(BinOp::And, l, r);
        }
        Ok(l)
    }
    fn not(&mut self) -> EsResult<Expr> {
        let s = self.pos();
        if self.eat_kw("NOT") {
            let e = self.not()?;
            let end = e.end;
            return Ok(Expr { kind: ExprKind::Not(Box::new(e)), start: s, end });
        }
        self.pred()
    }
    fn pred(&mut self) -> EsResult<Expr> {
        let l = self.add()?;
        let start = l.start;
        let op = match self.peek() {
            T::Op("=") => Some(BinOp::Eq),
            T::Op("<>") | T::Op("!=") => Some(BinOp::Neq),
            T::Op("<") => Some(BinOp::Lt),
            T::Op("<=") => Some(BinOp::Lte),
            T::Op(">") => Some(BinOp::Gt),
            T::Op(">=") => Some(BinOp::Gte),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let r = self.add()?;
            return Ok(bin(op, l, r));
        }
        if self.eat_kw("IS") {
            let neg = self.eat_kw("NOT");
            self.expect_kw("NULL")?;
            let end = self.t[self.i - 1].2;
            return Ok(Expr { kind: ExprKind::IsNull(Box::new(l), neg), start, end });
        }
        let neg = if self.is_kw("NOT") { self.bump(); true } else { false };
        if self.is_kw("LIKE") || self.is_kw("RLIKE") {
            let rlike = self.is_kw("RLIKE");
            self.bump();
            let (t, _, end) = self.bump();
            let T::Str(p) = t else { return Err(self.err("STRING")) };
            // SQL wildcards: `%` any run, `_` one character.
            let p = if rlike { p } else { p.replace('*', "\\*").replace('?', "\\?").replace('%', "*").replace('_', "?") };
            return Ok(Expr { kind: ExprKind::Like(Box::new(l), p, rlike, neg), start, end });
        }
        if self.eat_kw("IN") {
            if !self.eat_op("(") {
                return Err(self.err("'('"));
            }
            let mut list = Vec::new();
            loop {
                list.push(self.expr()?);
                if !self.eat_op(",") {
                    break;
                }
            }
            let end = self.t[self.i].2;
            if !self.eat_op(")") {
                return Err(self.err("')'"));
            }
            return Ok(Expr { kind: ExprKind::In(Box::new(l), list, neg), start, end });
        }
        if self.eat_kw("BETWEEN") {
            let lo = self.add()?;
            self.expect_kw("AND")?;
            let hi = self.add()?;
            let e = bin(BinOp::And, bin(BinOp::Gte, l.clone(), lo), bin(BinOp::Lte, l, hi));
            return Ok(if neg { Expr { start: e.start, end: e.end, kind: ExprKind::Not(Box::new(e)) } } else { e });
        }
        if neg {
            return Err(self.err("{'BETWEEN', 'IN', 'LIKE', 'RLIKE'}"));
        }
        Ok(l)
    }
    fn add(&mut self) -> EsResult<Expr> {
        let mut l = self.mul()?;
        loop {
            let op = match self.peek() {
                T::Op("+") => BinOp::Add,
                T::Op("-") => BinOp::Sub,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.mul()?;
            l = bin(op, l, r);
        }
    }
    fn mul(&mut self) -> EsResult<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                T::Op("*") => BinOp::Mul,
                T::Op("/") => BinOp::Div,
                T::Op("%") => BinOp::Mod,
                _ => return Ok(l),
            };
            self.bump();
            let r = self.unary()?;
            l = bin(op, l, r);
        }
    }
    fn unary(&mut self) -> EsResult<Expr> {
        let s = self.pos();
        if self.eat_op("-") {
            let e = self.unary()?;
            let end = e.end;
            return Ok(match e.kind {
                ExprKind::Lit(V::Int(i)) => Expr { kind: ExprKind::Lit(V::Int(-i)), start: s, end },
                ExprKind::Lit(V::Long(i)) => Expr { kind: ExprKind::Lit(V::Long(-i)), start: s, end },
                ExprKind::Lit(V::Double(d)) => Expr { kind: ExprKind::Lit(V::Double(-d)), start: s, end },
                _ => Expr { kind: ExprKind::Neg(Box::new(e)), start: s, end },
            });
        }
        let mut e = self.primary()?;
        while self.eat_op("::") {
            let ty = self.ident()?;
            let end = self.t[self.i - 1].2;
            e = Expr { kind: ExprKind::Cast(Box::new(e), sql_type(&ty)), start: s, end };
        }
        Ok(e)
    }
    fn primary(&mut self) -> EsResult<Expr> {
        let (t, s, e) = self.t[self.i].clone();
        let lit = |v: V| Expr { kind: ExprKind::Lit(v), start: s, end: e };
        match t {
            T::Op("?") => {
                self.bump();
                let v = self.params.get(self.next_param).cloned();
                self.next_param += 1;
                v.map(lit).ok_or_else(|| parsing(self.src, s, "Not enough actual parameters"))
            }
            T::Int(n) => {
                self.bump();
                Ok(lit(i32::try_from(n).map(V::Int).unwrap_or(V::Long(n))))
            }
            T::Dec(d) => {
                self.bump();
                Ok(lit(V::Double(d)))
            }
            T::Str(st) => {
                self.bump();
                Ok(lit(V::Str(st)))
            }
            T::Op("(") => {
                self.bump();
                let mut x = self.expr()?;
                let end = self.t[self.i].2;
                if !self.eat_op(")") {
                    return Err(self.err("')'"));
                }
                x.start = s;
                x.end = end;
                Ok(x)
            }
            T::Op("*") => {
                self.bump();
                Ok(Expr { kind: ExprKind::Star, start: s, end: e })
            }
            T::Word(w) if w.eq_ignore_ascii_case("NULL") => {
                self.bump();
                Ok(lit(V::Null))
            }
            T::Word(w) if w.eq_ignore_ascii_case("TRUE") || w.eq_ignore_ascii_case("FALSE") => {
                self.bump();
                Ok(lit(V::Bool(w.eq_ignore_ascii_case("TRUE"))))
            }
            T::Word(w) if w.eq_ignore_ascii_case("CASE") => {
                self.bump();
                let mut args = Vec::new();
                while self.eat_kw("WHEN") {
                    args.push(self.expr()?);
                    self.expect_kw("THEN")?;
                    args.push(self.expr()?);
                }
                if self.eat_kw("ELSE") {
                    args.push(self.expr()?);
                }
                let end = self.t[self.i].2;
                self.expect_kw("END")?;
                Ok(Expr { kind: ExprKind::Call("case".into(), args), start: s, end })
            }
            T::Word(w) if (w.eq_ignore_ascii_case("CAST") || w.eq_ignore_ascii_case("CONVERT")) && matches!(self.t[self.i + 1].0, T::Op("(")) => {
                self.bump();
                self.bump();
                let x = self.expr()?;
                if !self.eat_kw("AS") && !self.eat_op(",") {
                    return Err(self.err("'AS'"));
                }
                let ty = self.ident()?;
                let end = self.t[self.i].2;
                if !self.eat_op(")") {
                    return Err(self.err("')'"));
                }
                Ok(Expr { kind: ExprKind::Cast(Box::new(x), sql_type(&ty)), start: s, end })
            }
            T::Word(_) | T::Quoted(_) => {
                if matches!(t, T::Word(_)) && matches!(self.t[self.i + 1].0, T::Op("(")) {
                    let T::Word(name) = self.bump().0 else { unreachable!() };
                    self.bump();
                    let distinct = self.eat_kw("DISTINCT");
                    let mut args = Vec::new();
                    if !matches!(self.peek(), T::Op(")")) {
                        loop {
                            args.push(self.expr()?);
                            if !self.eat_op(",") {
                                break;
                            }
                        }
                    }
                    let end = self.t[self.i].2;
                    if !self.eat_op(")") {
                        return Err(self.err("')'"));
                    }
                    let mut fname = function_name(&name);
                    if distinct && fname == "count" {
                        fname = "count_distinct".into();
                    }
                    // Date part functions become DATE_EXTRACT.
                    if let Some(part) = date_part(&fname) {
                        let mut a = vec![Expr { kind: ExprKind::Lit(V::Str(part.into())), start: s, end }];
                        a.extend(args);
                        // SQL's datetime field functions return integers.
                        let mut inner = Expr { kind: ExprKind::Call("date_extract".into(), a), start: s, end };
                        if part == "day_of_week" {
                            // SQL counts from Sunday (1): `ISO % 7 + 1`.
                            let lit = |v: i32| Box::new(Expr { kind: ExprKind::Lit(V::Int(v)), start: s, end });
                            let m = Expr { kind: ExprKind::Bin(BinOp::Mod, Box::new(inner), lit(7)), start: s, end };
                            inner = Expr { kind: ExprKind::Bin(BinOp::Add, Box::new(m), lit(1)), start: s, end };
                        }
                        return Ok(Expr { kind: ExprKind::Call("to_integer".into(), vec![inner]), start: s, end });
                    }
                    return Ok(Expr { kind: ExprKind::Call(fname, args), start: s, end });
                }
                if let T::Word(w) = &t
                    && RESERVED.iter().any(|r| w.eq_ignore_ascii_case(r))
                {
                    return Err(self.err("{expression}"));
                }
                let name = self.ident()?;
                let end = self.t[self.i - 1].2;
                Ok(Expr { kind: ExprKind::Col(name), start: s, end })
            }
            _ => Err(self.err("{'(', 'CASE', 'CAST', 'NULL', 'TRUE', 'FALSE', STRING, INTEGER_VALUE, DECIMAL_VALUE, IDENTIFIER}")),
        }
    }
}

fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
    let (s, e) = (l.start, r.end);
    Expr { kind: ExprKind::Bin(op, Box::new(l), Box::new(r)), start: s, end: e }
}

/// `SHOW FUNCTIONS`: Elasticsearch's SQL function registry, in its order.
const SQL_FUNCTIONS: &[(&str, &str)] = &[
    ("AVG", "AGGREGATE"), ("COUNT", "AGGREGATE"), ("FIRST", "AGGREGATE"), ("FIRST_VALUE", "AGGREGATE"), ("LAST", "AGGREGATE"), ("LAST_VALUE", "AGGREGATE"),
    ("MAX", "AGGREGATE"), ("MIN", "AGGREGATE"), ("SUM", "AGGREGATE"), ("KURTOSIS", "AGGREGATE"), ("MAD", "AGGREGATE"), ("PERCENTILE", "AGGREGATE"),
    ("PERCENTILE_RANK", "AGGREGATE"), ("SKEWNESS", "AGGREGATE"), ("STDDEV_POP", "AGGREGATE"), ("STDDEV_SAMP", "AGGREGATE"), ("SUM_OF_SQUARES", "AGGREGATE"),
    ("VAR_POP", "AGGREGATE"), ("VAR_SAMP", "AGGREGATE"), ("HISTOGRAM", "GROUPING"), ("CASE", "CONDITIONAL"), ("COALESCE", "CONDITIONAL"), ("GREATEST", "CONDITIONAL"),
    ("IFNULL", "CONDITIONAL"), ("IIF", "CONDITIONAL"), ("ISNULL", "CONDITIONAL"), ("LEAST", "CONDITIONAL"), ("NULLIF", "CONDITIONAL"), ("NVL", "CONDITIONAL"),
    ("CURDATE", "SCALAR"), ("CURRENT_DATE", "SCALAR"), ("CURRENT_TIME", "SCALAR"), ("CURRENT_TIMESTAMP", "SCALAR"), ("CURTIME", "SCALAR"), ("DATEADD", "SCALAR"),
    ("DATEDIFF", "SCALAR"), ("DATEPART", "SCALAR"), ("DATETIME_FORMAT", "SCALAR"), ("DATETIME_PARSE", "SCALAR"), ("DATETRUNC", "SCALAR"), ("DATE_ADD", "SCALAR"),
    ("DATE_DIFF", "SCALAR"), ("DATE_FORMAT", "SCALAR"), ("DATE_PARSE", "SCALAR"), ("DATE_PART", "SCALAR"), ("DATE_TRUNC", "SCALAR"), ("DAY", "SCALAR"),
    ("DAYNAME", "SCALAR"), ("DAYOFMONTH", "SCALAR"), ("DAYOFWEEK", "SCALAR"), ("DAYOFYEAR", "SCALAR"), ("DAY_NAME", "SCALAR"), ("DAY_OF_MONTH", "SCALAR"),
    ("DAY_OF_WEEK", "SCALAR"), ("DAY_OF_YEAR", "SCALAR"), ("DOM", "SCALAR"), ("DOW", "SCALAR"), ("DOY", "SCALAR"), ("FORMAT", "SCALAR"), ("HOUR", "SCALAR"),
    ("HOUR_OF_DAY", "SCALAR"), ("IDOW", "SCALAR"), ("ISODAYOFWEEK", "SCALAR"), ("ISODOW", "SCALAR"), ("ISOWEEK", "SCALAR"), ("ISOWEEKOFYEAR", "SCALAR"),
    ("ISO_DAY_OF_WEEK", "SCALAR"), ("ISO_WEEK_OF_YEAR", "SCALAR"), ("IW", "SCALAR"), ("IWOY", "SCALAR"), ("MINUTE", "SCALAR"), ("MINUTE_OF_DAY", "SCALAR"),
    ("MINUTE_OF_HOUR", "SCALAR"), ("MONTH", "SCALAR"), ("MONTHNAME", "SCALAR"), ("MONTH_NAME", "SCALAR"), ("MONTH_OF_YEAR", "SCALAR"), ("NOW", "SCALAR"),
    ("QUARTER", "SCALAR"), ("SECOND", "SCALAR"), ("SECOND_OF_MINUTE", "SCALAR"), ("TIMESTAMPADD", "SCALAR"), ("TIMESTAMPDIFF", "SCALAR"), ("TIMESTAMP_ADD", "SCALAR"),
    ("TIMESTAMP_DIFF", "SCALAR"), ("TIME_PARSE", "SCALAR"), ("TODAY", "SCALAR"), ("TO_CHAR", "SCALAR"), ("WEEK", "SCALAR"), ("WEEK_OF_YEAR", "SCALAR"),
    ("YEAR", "SCALAR"), ("ABS", "SCALAR"), ("ACOS", "SCALAR"), ("ASIN", "SCALAR"), ("ATAN", "SCALAR"), ("ATAN2", "SCALAR"), ("CBRT", "SCALAR"), ("CEIL", "SCALAR"),
    ("CEILING", "SCALAR"), ("COS", "SCALAR"), ("COSH", "SCALAR"), ("COT", "SCALAR"), ("DEGREES", "SCALAR"), ("E", "SCALAR"), ("EXP", "SCALAR"), ("EXPM1", "SCALAR"),
    ("FLOOR", "SCALAR"), ("LOG", "SCALAR"), ("LOG10", "SCALAR"), ("MOD", "SCALAR"), ("PI", "SCALAR"), ("POWER", "SCALAR"), ("RADIANS", "SCALAR"), ("RAND", "SCALAR"),
    ("RANDOM", "SCALAR"), ("ROUND", "SCALAR"), ("SIGN", "SCALAR"), ("SIGNUM", "SCALAR"), ("SIN", "SCALAR"), ("SINH", "SCALAR"), ("SQRT", "SCALAR"), ("TAN", "SCALAR"),
    ("TRUNC", "SCALAR"), ("TRUNCATE", "SCALAR"), ("ASCII", "SCALAR"), ("BIT_LENGTH", "SCALAR"), ("CHAR", "SCALAR"), ("CHARACTER_LENGTH", "SCALAR"),
    ("CHAR_LENGTH", "SCALAR"), ("CONCAT", "SCALAR"), ("INSERT", "SCALAR"), ("LCASE", "SCALAR"), ("LEFT", "SCALAR"), ("LENGTH", "SCALAR"), ("LOCATE", "SCALAR"),
    ("LTRIM", "SCALAR"), ("OCTET_LENGTH", "SCALAR"), ("POSITION", "SCALAR"), ("REPEAT", "SCALAR"), ("REPLACE", "SCALAR"), ("RIGHT", "SCALAR"), ("RTRIM", "SCALAR"),
    ("SPACE", "SCALAR"), ("STARTS_WITH", "SCALAR"), ("SUBSTRING", "SCALAR"), ("TRIM", "SCALAR"), ("UCASE", "SCALAR"), ("CAST", "SCALAR"), ("CONVERT", "SCALAR"),
    ("DATABASE", "SCALAR"), ("USER", "SCALAR"), ("ST_ASTEXT", "SCALAR"), ("ST_ASWKT", "SCALAR"), ("ST_DISTANCE", "SCALAR"), ("ST_GEOMETRYTYPE", "SCALAR"),
    ("ST_GEOMFROMTEXT", "SCALAR"), ("ST_WKTTOSQL", "SCALAR"), ("ST_X", "SCALAR"), ("ST_Y", "SCALAR"), ("ST_Z", "SCALAR"), ("SCORE", "SCORE"),
];

fn sql_type(t: &str) -> String {
    match t.to_ascii_lowercase().as_str() {
        // `CAST(x AS VARCHAR)` is a text value in Elasticsearch SQL.
        "varchar" | "string" | "char" => "text".into(),
        "int" | "integer" | "smallint" | "tinyint" => "integer".into(),
        "bigint" | "long" => "long".into(),
        "real" | "float" | "double" | "decimal" | "numeric" => "double".into(),
        "timestamp" | "datetime" | "date" => "datetime".into(),
        "bool" | "boolean" => "boolean".into(),
        other => other.into(),
    }
}

fn function_name(n: &str) -> String {
    match n.to_ascii_lowercase().as_str() {
        "ucase" | "upper" => "to_upper".into(),
        // `HISTOGRAM(x, interval)` groups like ES|QL's `BUCKET(x, span)`.
        "histogram" => "bucket".into(),
        "lcase" | "lower" => "to_lower".into(),
        "char_length" | "character_length" => "length".into(),
        "ifnull" | "isnull" | "nvl" => "coalesce".into(),
        "power" => "pow".into(),
        "ceiling" => "ceil".into(),
        "truncate" => "round".into(),
        "stddev_pop" => "std_dev".into(),
        "current_timestamp" | "current_date" | "now" | "curdate" | "today" => "now".into(),
        other => other.into(),
    }
}

fn date_part(f: &str) -> Option<&'static str> {
    Some(match f {
        "year" => "year",
        "month" | "month_of_year" => "month_of_year",
        "day" | "day_of_month" | "dayofmonth" | "dom" => "day_of_month",
        "day_of_week" | "dayofweek" | "dow" => "day_of_week",
        "day_of_year" | "dayofyear" | "doy" => "day_of_year",
        "hour" | "hour_of_day" => "hour_of_day",
        "minute" | "minute_of_hour" => "minute_of_hour",
        "second" | "second_of_minute" => "second_of_minute",
        _ => return None,
    })
}

fn contains_agg(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call(n, args) => super::esql::value::AGGS.contains(&n.as_str()) || args.iter().any(contains_agg),
        ExprKind::Bin(_, a, b) => contains_agg(a) || contains_agg(b),
        ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::IsNull(x, _) | ExprKind::Cast(x, _) | ExprKind::Like(x, ..) => contains_agg(x),
        ExprKind::In(x, l, _) => contains_agg(x) || l.iter().any(contains_agg),
        _ => false,
    }
}

/// A parsed statement.
enum Stmt {
    Select { query: Query, table: String, limit: Option<usize>, star: bool },
    ShowTables(Option<String>),
    ShowColumns(String),
    /// `SHOW FUNCTIONS [LIKE pattern]` (the pattern as a glob).
    ShowFunctions(Option<String>),
}

fn parse(src: &str) -> EsResult<Stmt> {
    parse_with(src, &[])
}

/// The `params` of a request: plain values or `{"type": .., "value": ..}`.
fn params_of(v: Option<&Value>) -> Vec<V> {
    let one = |x: &Value| -> V {
        let x = match x {
            Value::Obj(m) if m.contains_key("value") => m.get("value").unwrap_or(&Value::Null),
            other => other,
        };
        match x {
            Value::Null => V::Null,
            Value::Bool(b) => V::Bool(*b),
            Value::Str(s) => V::Str(s.clone()),
            Value::Num(n) if n.is_integer() => n.as_i64().map(|i| i32::try_from(i).map(V::Int).unwrap_or(V::Long(i))).unwrap_or(V::Null),
            Value::Num(n) => V::Double(n.as_f64()),
            other => V::Str(other.to_json()),
        }
    };
    v.and_then(Value::as_array).map(|a| a.iter().map(one).collect()).unwrap_or_default()
}

/// SQL `LIKE` (`LikePattern`): `%` matches any run, `_` exactly one character, and every
/// other character itself (`*` and `?` included), case-sensitively.
fn sql_like(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Greedy matching with backtracking to the last `%`.
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || (p[pi] != '%' && p[pi] == t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '%')
}

fn parse_with(src: &str, params: &[V]) -> EsResult<Stmt> {
    let t = lex(src)?;
    let mut p = P { src, t, i: 0, params, next_param: 0 };
    if p.eat_kw("SHOW") {
        if p.eat_kw("TABLES") {
            let like = if p.eat_kw("LIKE") {
                match p.bump().0 {
                    T::Str(s) => Some(s.replace('%', "*").replace('_', "?")),
                    _ => return Err(p.err("STRING")),
                }
            } else if !matches!(p.peek(), T::Eof) {
                Some(p.table()?)
            } else {
                None
            };
            return Ok(Stmt::ShowTables(like));
        }
        if p.eat_kw("COLUMNS") {
            if !p.eat_kw("FROM") && !p.eat_kw("IN") {
                return Err(p.err("{'FROM', 'IN'}"));
            }
            return Ok(Stmt::ShowColumns(p.table()?));
        }
        if p.eat_kw("FUNCTIONS") {
            // Kept in SQL form: `%` any run, `_` one character, everything else literal.
            let like = if p.eat_kw("LIKE") {
                match p.bump().0 {
                    T::Str(s) => Some(s),
                    _ => return Err(p.err("STRING")),
                }
            } else {
                None
            };
            return Ok(Stmt::ShowFunctions(like));
        }
        return Err(p.err("{'TABLES', 'COLUMNS', 'FUNCTIONS'}"));
    }
    if p.eat_kw("DESCRIBE") || p.eat_kw("DESC") {
        return Ok(Stmt::ShowColumns(p.table()?));
    }
    if !p.eat_kw("SELECT") {
        return Err(p.err("{'(', 'DESCRIBE', 'EXPLAIN', 'SELECT', 'SHOW', 'SYS'}"));
    }
    let distinct = p.eat_kw("DISTINCT");
    p.eat_kw("ALL");
    let mut items: Vec<(Option<String>, Expr)> = Vec::new();
    let mut star = false;
    if matches!(p.peek(), T::Word(w) if w.eq_ignore_ascii_case("FROM")) {
        return Err(parsing(src, p.pos(), &format!("extraneous input '{}' expecting {{{SELECT_ITEM_START}}}", p.text())));
    }
    loop {
        if p.eat_op("*") {
            star = true;
        } else {
            let e = p.expr()?;
            let bare_alias = matches!(p.peek(), T::Word(w) if !RESERVED.iter().any(|r| w.eq_ignore_ascii_case(r))) || matches!(p.peek(), T::Quoted(_));
            let alias = if p.eat_kw("AS") || bare_alias {
                Some(p.ident()?)
            } else {
                None
            };
            items.push((alias, e));
        }
        if !p.eat_op(",") {
            break;
        }
    }
    let mut cmds = Vec::new();
    let table = if p.eat_kw("FROM") {
        let at = p.pos();
        let t = p.table()?;
        if p.eat_kw("AS") || matches!(p.peek(), T::Word(w) if !RESERVED.iter().any(|r| w.eq_ignore_ascii_case(r))) {
            p.ident().ok();
        }
        cmds.push(Cmd::From { sources: vec![t.clone()], metadata: Vec::new(), at });
        t
    } else {
        cmds.push(Cmd::Row(vec![Named { name: "__row".into(), expr: Expr { kind: ExprKind::Lit(V::Null), start: 0, end: 0 } }]));
        String::new()
    };
    if p.eat_kw("WHERE") {
        cmds.push(Cmd::Where(p.expr()?));
    }
    let mut group = Vec::new();
    if p.eat_kw("GROUP") {
        p.expect_kw("BY")?;
        loop {
            group.push(p.expr()?);
            if !p.eat_op(",") {
                break;
            }
        }
    }
    let having = if p.eat_kw("HAVING") { Some(p.expr()?) } else { None };
    let mut order = Vec::new();
    let mut order_explicit_nulls: Vec<bool> = Vec::new();
    if p.eat_kw("ORDER") {
        p.expect_kw("BY")?;
        loop {
            let e = p.expr()?;
            let asc = if p.eat_kw("DESC") { false } else { p.eat_kw("ASC"); true };
            let explicit = p.eat_kw("NULLS");
            let nulls_first = if explicit { p.eat_kw("FIRST") || { p.eat_kw("LAST"); false } } else { !asc };
            order.push(SortKey { expr: e, asc, nulls_first });
            order_explicit_nulls.push(explicit);
            if !p.eat_op(",") {
                break;
            }
        }
    }
    let mut limit = None;
    if p.eat_kw("LIMIT") {
        match p.bump().0 {
            T::Int(n) if n >= 0 => limit = Some(n as usize),
            _ => return Err(p.err("INTEGER_VALUE")),
        }
    }
    if !matches!(p.peek(), T::Eof) {
        return Err(parsing(src, p.pos(), &format!("extraneous input '{}' expecting <EOF>", p.text())));
    }
    let name_of = |a: &Option<String>, e: &Expr| a.clone().unwrap_or_else(|| if let ExprKind::Col(c) = &e.kind { c.clone() } else { src[e.start..e.end].to_string() });
    let aggregate = !group.is_empty() || items.iter().any(|(_, e)| contains_agg(e)) || having.is_some();
    let mut out_names: Vec<String> = Vec::new();
    // Where each output column was named, for errors.
    let out_at: Vec<usize> = items.iter().map(|(_, e)| e.start).collect();
    if aggregate || distinct {
        if star {
            return Err(EsError::new(400, "verification_exception", "Found 1 problem\nline 1:8: Cannot use * with GROUP BY or aggregates"));
        }
        let mut aggs = Vec::new();
        let mut by: Vec<Named> = Vec::new();
        for g in &group {
            // `GROUP BY <alias>` groups by the aliased select expression.
            if let ExprKind::Col(c) = &g.kind
                && let Some((a, e)) = items.iter().find(|(a, _)| a.as_deref() == Some(c.as_str()))
            {
                by.push(Named { name: a.clone().unwrap_or_default(), expr: e.clone() });
                continue;
            }
            // A group named by a select alias takes that name.
            let alias = items.iter().find(|(_, e)| src[e.start..e.end] == src[g.start..g.end]).and_then(|(a, _)| a.clone());
            by.push(Named { name: alias.unwrap_or_else(|| name_of(&None, g)), expr: g.clone() });
        }
        for (a, e) in &items {
            let n = name_of(a, e);
            out_names.push(n.clone());
            if contains_agg(e) {
                aggs.push(Named { name: n, expr: e.clone() });
            } else if distinct && group.is_empty() {
                by.push(Named { name: n, expr: e.clone() });
            } else if !by.iter().any(|b| b.name == n || src[b.expr.start..b.expr.end] == src[e.start..e.end]) {
                return Err(EsError::new(400, "verification_exception", format!("Found 1 problem\nline 1:{}: Cannot use non-grouped column [{}], expected [{}]", e.start + 1, n, by.iter().map(|b| b.name.clone()).collect::<Vec<_>>().join(", "))));
            }
        }
        let mut hidden = Vec::new();
        if let Some(h) = &having {
            // HAVING's aggregates become hidden outputs.
            collect_aggs(src, h, &mut aggs, &mut hidden);
        }
        for k in &order {
            collect_aggs(src, &k.expr, &mut aggs, &mut hidden);
        }
        cmds.push(Cmd::Stats { aggs, by: by.clone() });
        // Group outputs of the select list that aren't plain group expressions are evaluated after.
        for (a, e) in &items {
            let n = name_of(a, e);
            if !contains_agg(e) && !by.iter().any(|b| b.name == n) {
                cmds.push(Cmd::Eval(vec![Named { name: n, expr: e.clone() }]));
            }
        }
        if let Some(h) = having {
            cmds.push(Cmd::Where(rewrite_aggs(src, h)));
        }
    } else {
        for (a, e) in &items {
            let n = name_of(a, e);
            if !matches!(&e.kind, ExprKind::Col(c) if *c == n) {
                cmds.push(Cmd::Eval(vec![Named { name: n.clone(), expr: e.clone() }]));
            }
            out_names.push(n);
        }
    }
    if aggregate && !group.is_empty() {
        // Groups come from a composite aggregation: ordered by key, the null group smallest.
        for (k, explicit) in order.iter_mut().zip(&order_explicit_nulls) {
            if !*explicit && !contains_agg(&k.expr) {
                k.nulls_first = k.asc;
            }
        }
        if order.is_empty() {
            for g in group.iter().filter(|g| matches!(g.kind, ExprKind::Col(_))) {
                order.push(SortKey { expr: g.clone(), asc: true, nulls_first: true });
            }
        }
    }
    if !order.is_empty() {
        let keys = order.into_iter().map(|k| SortKey { expr: rewrite_aggs(src, k.expr), ..k }).collect();
        cmds.push(Cmd::Sort(keys));
    }
    if !star {
        cmds.push(Cmd::Keep(out_names.iter().enumerate().map(|(i, n)| (n.clone(), out_at.get(i).copied().unwrap_or(0))).collect()));
    }
    if let Some(l) = limit {
        cmds.push(Cmd::Limit(l));
    }
    Ok(Stmt::Select { query: Query { src: src.to_string(), cmds, sql: true, locale: None, settings: Default::default() }, table, limit, star })
}

/// Collects `HAVING`'s aggregate calls as named outputs (named by their text).
fn collect_aggs(src: &str, e: &Expr, aggs: &mut Vec<Named>, hidden: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Call(n, args) if super::esql::value::AGGS.contains(&n.as_str()) => {
            let name = src[e.start..e.end].to_string();
            // Matched by output name only: an aliased `COUNT(*) c` still needs a `COUNT(*)`
            // output for the rewritten reference.
            if !aggs.iter().any(|a| a.name == name) {
                aggs.push(Named { name: name.clone(), expr: e.clone() });
                hidden.push(name);
            }
            let _ = args;
        }
        ExprKind::Call(_, args) => args.iter().for_each(|a| collect_aggs(src, a, aggs, hidden)),
        ExprKind::Bin(_, a, b) => {
            collect_aggs(src, a, aggs, hidden);
            collect_aggs(src, b, aggs, hidden);
        }
        ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::IsNull(x, _) | ExprKind::Cast(x, _) => collect_aggs(src, x, aggs, hidden),
        _ => {}
    }
}

/// Replaces aggregate calls by references to the outputs named by their text.
fn rewrite_aggs(src: &str, e: Expr) -> Expr {
    let (s, en) = (e.start, e.end);
    let kind = match e.kind {
        ExprKind::Call(n, _) if super::esql::value::AGGS.contains(&n.as_str()) => ExprKind::Col(src[s..en].to_string()),
        ExprKind::Call(n, args) => ExprKind::Call(n, args.into_iter().map(|a| rewrite_aggs(src, a)).collect()),
        ExprKind::Bin(op, a, b) => ExprKind::Bin(op, Box::new(rewrite_aggs(src, *a)), Box::new(rewrite_aggs(src, *b))),
        ExprKind::Not(x) => ExprKind::Not(Box::new(rewrite_aggs(src, *x))),
        ExprKind::Neg(x) => ExprKind::Neg(Box::new(rewrite_aggs(src, *x))),
        other => other,
    };
    Expr { kind, start: s, end: en }
}

/// Field types of a table (`field_caps`), to print SQL's type names and hide multi-fields.
/// Elasticsearch runs `GROUP BY`, aggregates and `ORDER BY` on fields as aggregations and
/// sorts, which need doc values: a field without them fails the search ("all shards failed",
/// `Can't load fielddata on [f] ...`). The same request through the core search API fails
/// the same way here, so its error is the answer.
async fn check_doc_values(ctx: &Ctx<'_>, table: &str, q: &Query) -> EsResult<()> {
    use std::collections::HashSet;
    let (mut grouped, mut sorted): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    let mut defined: HashSet<String> = HashSet::new();
    // `COUNT(f)` counts with an `exists` query, which needs no doc values.
    fn uncounted(e: &Expr, out: &mut HashSet<String>) {
        match &e.kind {
            ExprKind::Call(f, _) if f.eq_ignore_ascii_case("count") => {}
            ExprKind::Call(_, args) => args.iter().for_each(|a| uncounted(a, out)),
            ExprKind::Bin(_, a, b) => {
                uncounted(a, out);
                uncounted(b, out);
            }
            ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::Cast(x, _) => uncounted(x, out),
            _ => exec::refs(e, out),
        }
    }
    let source_refs = |e: &Expr, defined: &HashSet<String>, out: &mut Vec<String>| {
        let mut r = HashSet::new();
        uncounted(e, &mut r);
        let mut r: Vec<String> = r.into_iter().filter(|c| !defined.contains(c)).collect();
        r.sort();
        out.extend(r);
    };
    for c in &q.cmds {
        match c {
            Cmd::Eval(ns) => defined.extend(ns.iter().map(|n| n.name.clone())),
            Cmd::Rename(rs) => defined.extend(rs.iter().map(|r| r.1.clone())),
            Cmd::Stats { aggs, by } => {
                for n in aggs.iter().chain(by) {
                    source_refs(&n.expr, &defined, &mut grouped);
                }
                defined.extend(aggs.iter().chain(by).map(|n| n.name.clone()));
            }
            Cmd::Sort(keys) => {
                for k in keys {
                    source_refs(&k.expr, &defined, &mut sorted);
                }
            }
            _ => {}
        }
    }
    if grouped.is_empty() && sorted.is_empty() {
        return Ok(());
    }
    let names: Vec<&String> = grouped.iter().chain(&sorted).collect();
    let fields = names.iter().map(|n| super::enc(n)).collect::<Vec<_>>().join(",");
    let Ok(caps) = internal::call(ctx.cluster, "GET", &format!("/{table}/_field_caps?fields={fields}&ignore_unavailable=true"), None).await else { return Ok(()) };
    let without_dv = |f: &str| {
        caps.pointer(&["fields", f]).and_then(Value::as_object).is_some_and(|types| {
            types.iter().any(|(t, c)| !matches!(t.as_str(), "text" | "match_only_text" | "object" | "nested") && c.get("aggregatable").and_then(Value::as_bool) == Some(false))
        })
    };
    let probe = if let Some(f) = grouped.iter().find(|f| without_dv(f)) {
        obj! { "size" => 0, "aggs" => obj! { "a" => obj! { "terms" => obj! { "field" => f.as_str() } } } }
    } else if let Some(f) = sorted.iter().find(|f| without_dv(f)) {
        obj! { "size" => 1, "track_total_hits" => false, "sort" => crate::arr![obj! { f.as_str() => "asc" }] }
    } else {
        return Ok(());
    };
    internal::call(ctx.cluster, "POST", &format!("/{table}/_search?ignore_unavailable=true"), Some(&probe)).await.map(|_| ())
}

async fn mapping_types(ctx: &Ctx<'_>, table: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if table.is_empty() {
        return out;
    }
    if let Ok(caps) = internal::call(ctx.cluster, "GET", &format!("/{table}/_field_caps?fields=*&ignore_unavailable=true"), None).await
        && let Some(Value::Obj(fields)) = caps.get("fields")
    {
        for (name, types) in fields {
            if let Some(t) = types.as_object().and_then(|m| m.keys().next()) {
                out.insert(name.clone(), t.clone());
            }
        }
    }
    out
}

fn sql_type_name(col: &exec::Col, mapping: Option<&String>) -> &'static str {
    match (col.ty, mapping.map(String::as_str)) {
        (DataType::Double, Some("float")) => "float",
        (DataType::Double, Some("half_float")) => "half_float",
        (DataType::Double, Some("scaled_float")) => "scaled_float",
        (DataType::Integer, Some("short")) => "short",
        (DataType::Integer, Some("byte")) => "byte",
        (DataType::Date, _) => "datetime",
        (DataType::Null, _) => "null",
        (t, _) => t.name(),
    }
}

/// A finished result: columns with SQL types, rows as JSON.
struct Result {
    columns: Vec<(String, String)>,
    rows: Vec<Vec<Value>>,
    text_rows: Vec<Vec<String>>,
}

async fn run(ctx: &Ctx<'_>, stmt: Stmt, filter: Option<Value>, lenient: bool) -> EsResult<Result> {
    match stmt {
        Stmt::ShowTables(like) => {
            let st = ctx.state();
            let mut rows = Vec::new();
            let mut names: Vec<(String, &str)> = st.metadata.indices.keys().filter(|k| !k.starts_with('.')).map(|k| (k.clone(), "TABLE")).collect();
            for (alias, _) in st.aliases() {
                if !alias.starts_with('.') {
                    names.push((alias.to_string(), "VIEW"));
                }
            }
            names.sort();
            names.dedup();
            for (n, kind) in names {
                if like.as_ref().is_some_and(|p| !crate::xcontent::filter::glob_match(p, &n)) {
                    continue;
                }
                rows.push(vec![Value::from(ctx.cluster.name.clone()), Value::from(n), Value::from(kind), Value::from(if kind == "TABLE" { "INDEX" } else { "ALIAS" })]);
            }
            let text_rows = rows.iter().map(|r| r.iter().map(Value::to_es_string).collect()).collect();
            Ok(Result { columns: vec![("catalog".into(), "keyword".into()), ("name".into(), "keyword".into()), ("type".into(), "keyword".into()), ("kind".into(), "keyword".into())], rows, text_rows })
        }
        Stmt::ShowColumns(table) => {
            let types = mapping_types(ctx, &table).await;
            let mut names: Vec<&String> = types.keys().filter(|k| !k.starts_with('_')).collect();
            names.sort();
            let mut rows = Vec::new();
            for n in names {
                let t = &types[n];
                if t == "object" || t == "nested" {
                    continue;
                }
                let ty = DataType::of_mapping(t);
                let sql = match (ty, t.as_str()) {
                    (DataType::Keyword | DataType::Text, _) => "VARCHAR",
                    (_, "long") => "BIGINT",
                    (_, "integer") => "INTEGER",
                    (_, "short") => "SMALLINT",
                    (_, "byte") => "TINYINT",
                    (_, "double") => "DOUBLE",
                    (_, "float" | "half_float") => "REAL",
                    (_, "scaled_float") => "DOUBLE",
                    (DataType::Date, _) => "TIMESTAMP",
                    (DataType::Boolean, _) => "BOOLEAN",
                    (DataType::Ip | DataType::Version, _) => "VARCHAR",
                    _ => "OTHER",
                };
                rows.push(vec![Value::from(n.clone()), Value::from(sql), Value::from(if ty == DataType::Date { "datetime" } else { t.as_str() })]);
            }
            let text_rows = rows.iter().map(|r| r.iter().map(Value::to_es_string).collect()).collect();
            Ok(Result { columns: vec![("column".into(), "keyword".into()), ("type".into(), "keyword".into()), ("mapping".into(), "keyword".into())], rows, text_rows })
        }
        Stmt::ShowFunctions(like) => {
            let mut rows = Vec::new();
            for (f, kind) in SQL_FUNCTIONS {
                if like.as_ref().is_some_and(|p| !sql_like(p, f)) {
                    continue;
                }
                rows.push(vec![Value::from(*f), Value::from(*kind)]);
            }
            let text_rows = rows.iter().map(|r| r.iter().map(Value::to_es_string).collect()).collect();
            Ok(Result { columns: vec![("name".into(), "keyword".into()), ("type".into(), "keyword".into())], rows, text_rows })
        }
        Stmt::Select { query, table, star, .. } => {
            check_doc_values(ctx, &table, &query).await?;
            let types = mapping_types(ctx, &table).await;
            let mut w = Warnings::default();
            let mut q = query;
            // SQL returns every row unless limited (paged by fetch_size): lift ES|QL's default.
            if !q.cmds.iter().any(|c| matches!(c, Cmd::Limit(_))) {
                q.cmds.push(Cmd::Limit(exec::MAX_LIMIT));
            }
            let from_at = match q.cmds.first() {
                Some(Cmd::From { at, .. }) => super::esql::parser::location(&q.src, *at),
                _ => (1, 1),
            };
            let (mut t, _) = exec::execute(ctx.cluster, q, filter, &mut w).await.map_err(|e| {
                if e.kind == "verification_exception" && e.reason().starts_with("Unknown index") {
                    EsError::new(400, "verification_exception", format!("Found 1 problem\nline {}:{}: Unknown index [{table}]", from_at.0, from_at.1))
                } else {
                    e
                }
            })?;
            if star {
                // Multi-fields of text fields aren't columns of their own in SQL.
                let keep: Vec<usize> = (0..t.cols.len())
                    .filter(|i| {
                        let n = &t.cols[*i].name;
                        !n.rsplit_once('.').is_some_and(|(parent, _)| types.get(parent).is_some_and(|ty| ty == "text" || ty == "keyword"))
                    })
                    .collect();
                t = Table { cols: keep.iter().map(|i| t.cols[*i].clone()).collect(), rows: t.rows.iter().map(|r| keep.iter().map(|i| r[*i].clone()).collect()).collect() };
            }
            // SQL rejects multi-valued fields unless `field_multi_value_leniency` is set.
            if !lenient
                && let Some(i) = (0..t.cols.len()).find(|i| t.rows.iter().any(|r| matches!(r[*i], V::Multi(_))))
            {
                return Err(EsError::new(400, "invalid_argument_exception", format!("Arrays (returned by [{}]) are not supported", t.cols[i].name)));
            }
            let columns = t.cols.iter().map(|c| (c.name.clone(), sql_type_name(c, types.get(&c.name)).to_string())).collect();
            let rows = t.rows.iter().map(|r| r.iter().enumerate().map(|(i, v)| sql_json(v, t.cols[i].ty)).collect()).collect();
            let text_rows = t.rows.iter().map(|r| r.iter().enumerate().map(|(i, v)| exec::cell_text(v, t.cols[i].ty, "txt")).collect()).collect();
            Ok(Result { columns, rows, text_rows })
        }
    }
}

fn sql_json(v: &V, ty: DataType) -> Value {
    match (v, ty) {
        (V::Double(d), _) if d.fract() == 0.0 && d.is_finite() => Value::from(*d),
        _ => v.to_json(ty),
    }
}

const CURSOR_KEEP_MS: i64 = 45_000;

fn render(ctx: &Ctx, columns: Option<&[(String, String)]>, rows: &[Vec<Value>], text_rows: &[Vec<String>], cursor: Option<&str>, head: Option<Map>) -> EsResult<Reply> {
    let columnar = ctx.body().ok().flatten().and_then(|b| b.get("columnar").and_then(Value::as_bool)).unwrap_or(false);
    let format = ctx.query("format").map(str::to_ascii_lowercase).unwrap_or_else(|| "json".into());
    match format.as_str() {
        "txt" | "csv" | "tsv" => {
            let names: Vec<String> = columns.map(|c| c.iter().map(|(n, _)| n.clone()).collect()).unwrap_or_default();
            let body = exec::render_text(&names, text_rows, &format, ctx.query("delimiter").and_then(|d| d.chars().next()).unwrap_or(','), columns.is_some());
            let ct = match format.as_str() {
                "txt" => "text/plain; charset=UTF-8",
                "csv" => "text/csv; charset=utf-8; header=present",
                _ => "text/tab-separated-values; charset=utf-8",
            };
            // Text formats carry no cursor (Elasticsearch sends it in a header).
            let _ = cursor;
            Ok(Reply::Raw(200, ct.into(), bytes::Bytes::from(body)))
        }
        "json" | "yaml" | "cbor" | "smile" => {
            let mut m = head.unwrap_or_default();
            if let Some(cols) = columns {
                m.insert("columns".into(), Value::Arr(cols.iter().map(|(n, t)| obj! { "name" => n.clone(), "type" => t.clone() }).collect()));
            }
            if columnar {
                let n = columns.map(|c| c.len()).or_else(|| rows.first().map(Vec::len)).unwrap_or(0);
                m.insert("values".into(), Value::Arr((0..n).map(|i| Value::Arr(rows.iter().map(|r| r.get(i).cloned().unwrap_or(Value::Null)).collect())).collect()));
            } else {
                m.insert("rows".into(), Value::Arr(rows.iter().map(|r| Value::Arr(r.clone())).collect()));
            }
            if let Some(c) = cursor {
                m.insert("cursor".into(), c.into());
            }
            Reply::ok(Value::Obj(m))
        }
        other => Err(EsError::illegal_argument(format!(
            "Invalid request content type: Accept=[{}], Content-Type=[{}], format=[{other}]",
            ctx.request.header("accept").unwrap_or("null"),
            ctx.request.header("content-type").unwrap_or("null")
        ))),
    }
}

/// Splits a result at `fetch_size`, keeping the rest behind a cursor.
fn page(ctx: &Ctx, result: &Result, fetch: usize) -> Option<String> {
    if result.rows.len() <= fetch {
        return None;
    }
    let cursor = async_results::new_id();
    let rest = obj! {
        "rows" => Value::Arr(result.rows[fetch..].iter().map(|r| Value::Arr(r.clone())).collect()),
        "text" => Value::Arr(result.text_rows[fetch..].iter().map(|r| Value::Arr(r.iter().map(|s| Value::from(s.clone())).collect())).collect()),
        "fetch" => fetch,
    };
    ctx.cluster.xpack.results.put(cursor.clone(), Entry { kind: Kind::SqlCursor, response: Value::Null, start_ms: ctx.now_ms, expiration_ms: ctx.now_ms + CURSOR_KEEP_MS, bytes: 0, extra: rest }, ctx.now_ms);
    Some(cursor)
}

/// A cursor roll never issued: Elasticsearch fails to inflate it.
fn foreign_cursor(cursor: &str) -> bool {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cursor).map(|b| b.len() != 30).unwrap_or(true)
}

fn bad_cursor() -> EsError {
    EsError::illegal_argument("Input stream is not compressed with DEFLATE!")
}

fn next_page(ctx: &Ctx, cursor: &str) -> EsResult<Reply> {
    let Some(e) = ctx.cluster.xpack.results.get(cursor, Kind::SqlCursor, ctx.now_ms) else {
        if foreign_cursor(cursor) {
            return Err(bad_cursor());
        }
        return Err(EsError::illegal_argument("Unknown or expired cursor"));
    };
    ctx.cluster.xpack.results.remove(cursor, Kind::SqlCursor);
    let rows: Vec<Vec<Value>> = e.extra.get("rows").and_then(Value::as_array).map(|a| a.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect()).unwrap_or_default();
    let text: Vec<Vec<String>> = e.extra.get("text").and_then(Value::as_array).map(|a| a.iter().map(|r| r.as_array().map(|x| x.iter().map(Value::to_es_string).collect()).unwrap_or_default()).collect()).unwrap_or_default();
    let fetch = e.extra.get("fetch").and_then(Value::as_i64).unwrap_or(1000).max(1) as usize;
    let res = Result { columns: Vec::new(), rows, text_rows: text };
    let next = page(ctx, &res, fetch);
    let n = res.rows.len().min(fetch);
    render(ctx, None, &res.rows[..n], &res.text_rows[..n], next.as_deref(), None)
}

/// `SqlQueryRequest`'s fields (`sql/query`).
const QUERY_FIELDS: &[&str] = &[
    "query", "cursor", "fetch_size", "filter", "time_zone", "request_timeout", "page_timeout", "columnar", "field_multi_value_leniency",
    "index_include_frozen", "binary_format", "params", "runtime_mappings", "wait_for_completion_timeout", "keep_on_completion", "keep_alive",
    "catalog", "allow_partial_search_results", "project_routing", "mode", "client_id", "version",
];

pub async fn query(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    if let Some(k) = body.as_object().and_then(|m| m.keys().find(|k| !QUERY_FIELDS.contains(&k.as_str()))) {
        return Err(crate::xpack::unknown_field(ctx, "sql/query", k));
    }
    if let Some(c) = body.get("cursor").and_then(Value::as_str) {
        return next_page(ctx, c);
    }
    let Some(sql) = body.get("query").and_then(Value::as_str) else {
        return Err(EsError::action_request_validation(&["one of [query] or [cursor] is required".into()]));
    };
    let fetch = body.get("fetch_size").and_then(Value::as_i64).unwrap_or(1000);
    if fetch <= 0 {
        return Err(EsError::action_request_validation(&["[fetch_size] must be greater than 0.".into()]));
    }
    let stmt = parse_with(sql, &params_of(body.get("params")))?;
    let lenient = body.get("field_multi_value_leniency").and_then(Value::as_bool).unwrap_or(false);
    let result = run(ctx, stmt, body.get("filter").cloned(), lenient).await?;
    let is_async = body.get("wait_for_completion_timeout").is_some() || body.get("keep_on_completion").and_then(Value::as_bool) == Some(true);
    let fetch = fetch as usize;
    let cursor = page(ctx, &result, fetch);
    let n = result.rows.len().min(fetch);
    if is_async {
        let keep = body.get("keep_alive").map(|k| crate::units::TimeValue::parse(&k.to_es_string(), "keep_alive")).transpose()?.map_or(5 * 24 * 3600 * 1000, |t| t.as_millis());
        let id = async_results::async_id(&ctx.cluster.node.id);
        let mut m = Map::new();
        m.insert("columns".into(), Value::Arr(result.columns.iter().map(|(n, t)| obj! { "name" => n.clone(), "type" => t.clone() }).collect()));
        m.insert("rows".into(), Value::Arr(result.rows[..n].iter().map(|r| Value::Arr(r.clone())).collect()));
        if let Some(c) = &cursor {
            m.insert("cursor".into(), c.clone().into());
        }
        ctx.cluster.xpack.results.put(id.clone(), Entry { kind: Kind::Sql, response: Value::Obj(m), start_ms: ctx.now_ms, expiration_ms: ctx.now_ms + keep, bytes: 0, extra: Value::Null }, ctx.now_ms);
        let mut head = Map::new();
        head.insert("id".into(), id.into());
        head.insert("is_partial".into(), false.into());
        head.insert("is_running".into(), false.into());
        return render(ctx, Some(&result.columns), &result.rows[..n], &result.text_rows[..n], cursor.as_deref(), Some(head));
    }
    render(ctx, Some(&result.columns), &result.rows[..n], &result.text_rows[..n], cursor.as_deref(), None)
}

pub fn clear_cursor(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let Some(c) = body.get("cursor").and_then(Value::as_str) else {
        return Err(EsError::action_request_validation(&["[cursor] is required".into()]));
    };
    let removed = ctx.cluster.xpack.results.remove(c, Kind::SqlCursor);
    if !removed && foreign_cursor(c) {
        return Err(bad_cursor());
    }
    Reply::ok(obj! { "succeeded" => removed })
}

/// The response format of a request without a body (`SqlMediaTypeParser`): `format`, else the
/// `Accept` header, else `Content-Type`; nothing usable is a 400 before anything is looked up.
fn negotiated_format(ctx: &Ctx) -> EsResult<&'static str> {
    fn of_media(m: &str) -> Option<&'static str> {
        let base = m.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        Some(match base.as_str() {
            "application/json" | "application/vnd.elasticsearch+json" => "json",
            "application/yaml" | "application/vnd.elasticsearch+yaml" => "yaml",
            "application/cbor" | "application/vnd.elasticsearch+cbor" => "cbor",
            "application/smile" | "application/vnd.elasticsearch+smile" => "smile",
            "text/plain" | "text/vnd.elasticsearch+plain" => "txt",
            "text/csv" | "text/vnd.elasticsearch+csv" => "csv",
            "text/tab-separated-values" | "text/vnd.elasticsearch+tab-separated-values" => "tsv",
            _ => return None,
        })
    }
    let accept = ctx.request.header("accept");
    let ct = ctx.request.header("content-type");
    let found = match ctx.query("format") {
        Some(f) => ["json", "yaml", "cbor", "smile", "txt", "csv", "tsv"].into_iter().find(|x| x.eq_ignore_ascii_case(f)),
        None => accept.and_then(of_media).or_else(|| ct.and_then(of_media)),
    };
    found.ok_or_else(|| {
        EsError::illegal_argument(format!(
            "Invalid request content type: Accept=[{}], Content-Type=[{}], format=[{}]",
            accept.unwrap_or("null"),
            ct.unwrap_or("null"),
            ctx.query("format").unwrap_or("null")
        ))
    })
}

pub fn get_async(ctx: &Ctx) -> EsResult<Reply> {
    let format = negotiated_format(ctx)?;
    let id = ctx.path_param("id").unwrap_or_default();
    async_results::check_id(id)?;
    let Some(e) = ctx.cluster.xpack.results.get(id, Kind::Sql, ctx.now_ms) else { return Err(EsError::resource_not_found(id.to_string())) };
    if matches!(format, "txt" | "csv" | "tsv") {
        let names: Vec<String> = e.response.get("columns").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("name").map(Value::to_es_string)).collect();
        let rows: Vec<Vec<String>> = e
            .response
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|r| r.as_array().into_iter().flatten().map(|v| if v.is_null() { String::new() } else { v.to_es_string() }).collect())
            .collect();
        let delimiter = ctx.query("delimiter").and_then(|d| d.chars().next()).unwrap_or(',');
        let body = exec::render_text(&names, &rows, format, delimiter, !names.is_empty());
        let ct = match format {
            "txt" => "text/plain; charset=UTF-8",
            "csv" => "text/csv; charset=utf-8; header=present",
            _ => "text/tab-separated-values; charset=utf-8",
        };
        return Ok(Reply::Raw(200, ct.into(), bytes::Bytes::from(body)));
    }
    let mut m = Map::new();
    m.insert("id".into(), id.into());
    m.insert("is_partial".into(), false.into());
    m.insert("is_running".into(), false.into());
    if let Value::Obj(r) = e.response {
        m.extend(r);
    }
    Reply::ok(Value::Obj(m))
}

pub fn get_async_status(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    async_results::check_id(id)?;
    let Some(e) = ctx.cluster.xpack.results.get(id, Kind::Sql, ctx.now_ms) else { return Err(EsError::resource_not_found(id.to_string())) };
    Reply::ok(obj! {
        "id" => id, "is_running" => false, "is_partial" => false, "start_time_in_millis" => e.start_ms,
        "expiration_time_in_millis" => e.expiration_ms, "completion_status" => 200,
    })
}

pub fn delete_async(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    async_results::check_id(id)?;
    if ctx.cluster.xpack.results.remove(id, Kind::Sql) { Reply::acknowledged() } else { Err(EsError::resource_not_found(id.to_string())) }
}

/// `POST _sql/translate`: the search a `SELECT` reads its table with.
pub async fn translate(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let Some(sql) = body.get("query").and_then(Value::as_str) else {
        return Err(EsError::action_request_validation(&["[query] is required".into()]));
    };
    let Stmt::Select { query, table, limit, star } = parse(sql)? else {
        return Err(EsError::illegal_argument("Can only translate SELECT statements"));
    };
    let types = mapping_types(ctx, &table).await;
    let mut out = Map::new();
    out.insert("size".into(), limit.unwrap_or(1000).into());
    let mut fields_for = Vec::new();
    let mut where_q = None;
    let mut sort = Vec::new();
    for c in &query.cmds {
        match c {
            Cmd::Where(e) => where_q = translate_expr(e, &types),
            Cmd::Keep(names) => fields_for = names.iter().map(|(n, _)| n.clone()).collect(),
            Cmd::Sort(keys) => {
                for k in keys {
                    if let ExprKind::Col(f) = &k.expr.kind {
                        let f = if types.get(f).is_some_and(|t| t == "text") && types.contains_key(&format!("{f}.keyword")) { format!("{f}.keyword") } else { f.clone() };
                        sort.push(obj! { f => obj! { "order" => if k.asc { "asc" } else { "desc" }, "missing" => if k.nulls_first { "_first" } else { "_last" }, "unmapped_type" => types.get(&f).cloned().unwrap_or_else(|| "keyword".into()) } });
                    }
                }
            }
            _ => {}
        }
    }
    if star {
        let mut names: Vec<&String> = types.keys().filter(|k| !k.starts_with('_') && !matches!(types[*k].as_str(), "object" | "nested")).collect();
        names.sort();
        fields_for = names.into_iter().filter(|n| !n.rsplit_once('.').is_some_and(|(p, _)| types.get(p).is_some_and(|t| t == "text" || t == "keyword"))).cloned().collect();
    }
    if let Some(q) = where_q {
        out.insert("query".into(), q);
    }
    out.insert("_source".into(), false.into());
    out.insert(
        "fields".into(),
        Value::Arr(fields_for.iter().map(|f| if types.get(f).is_some_and(|t| t == "date") { obj! { "field" => f.clone(), "format" => "strict_date_optional_time_nanos" } } else { obj! { "field" => f.clone() } }).collect()),
    );
    if sort.is_empty() {
        sort.push(obj! { "_doc" => obj! { "order" => "asc" } });
    }
    out.insert("sort".into(), Value::Arr(sort));
    out.insert("track_total_hits".into(), (-1).into());
    Reply::ok(Value::Obj(out))
}

fn translate_expr(e: &Expr, types: &std::collections::HashMap<String, String>) -> Option<Value> {
    let lit = |x: &Expr| if let ExprKind::Lit(v) = &x.kind { Some(v.to_json(v.type_of())) } else { None };
    match &e.kind {
        ExprKind::Bin(BinOp::And, a, b) => Some(obj! { "bool" => obj! { "must" => crate::arr![translate_expr(a, types)?, translate_expr(b, types)?], "boost" => 1.0 } }),
        ExprKind::Bin(BinOp::Or, a, b) => Some(obj! { "bool" => obj! { "should" => crate::arr![translate_expr(a, types)?, translate_expr(b, types)?], "boost" => 1.0 } }),
        ExprKind::Not(x) => Some(obj! { "bool" => obj! { "must_not" => crate::arr![translate_expr(x, types)?], "boost" => 1.0 } }),
        ExprKind::Bin(op, a, b) => {
            let ExprKind::Col(f) = &a.kind else { return None };
            let v = lit(b)?;
            let f = if types.get(f).is_some_and(|t| t == "text") && types.contains_key(&format!("{f}.keyword")) { format!("{f}.keyword") } else { f.clone() };
            Some(match op {
                BinOp::Eq => obj! { "term" => obj! { f => obj! { "value" => v, "boost" => 1.0 } } },
                BinOp::Neq => obj! { "bool" => obj! { "must_not" => crate::arr![obj! { "term" => obj! { f => obj! { "value" => v, "boost" => 1.0 } } }], "boost" => 1.0 } },
                BinOp::Lt => obj! { "range" => obj! { f => obj! { "lt" => v, "boost" => 1.0 } } },
                BinOp::Lte => obj! { "range" => obj! { f => obj! { "lte" => v, "boost" => 1.0 } } },
                BinOp::Gt => obj! { "range" => obj! { f => obj! { "gt" => v, "boost" => 1.0 } } },
                BinOp::Gte => obj! { "range" => obj! { f => obj! { "gte" => v, "boost" => 1.0 } } },
                _ => return None,
            })
        }
        ExprKind::IsNull(x, neg) => {
            let ExprKind::Col(f) = &x.kind else { return None };
            let ex = obj! { "exists" => obj! { "field" => f.clone(), "boost" => 1.0 } };
            Some(if *neg { ex } else { obj! { "bool" => obj! { "must_not" => crate::arr![ex], "boost" => 1.0 } } })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_become_pipelines() {
        let Stmt::Select { query, table, .. } = parse(r#"SELECT author, COUNT(*) c, MAX(page_count) FROM "sql_test" WHERE page_count > 1 GROUP BY author HAVING COUNT(*) > 0 ORDER BY author LIMIT 5"#).unwrap() else { panic!() };
        assert_eq!(table, "sql_test");
        let kinds: Vec<&str> = query.cmds.iter().map(|c| match c {
            Cmd::From { .. } => "from",
            Cmd::Where(_) => "where",
            Cmd::Stats { .. } => "stats",
            Cmd::Eval(_) => "eval",
            Cmd::Sort(_) => "sort",
            Cmd::Keep(_) => "keep",
            Cmd::Limit(_) => "limit",
            _ => "other",
        }).collect();
        assert_eq!(kinds, ["from", "where", "stats", "where", "sort", "keep", "limit"]);
        let Cmd::Stats { aggs, by } = &query.cmds[2] else { panic!() };
        assert_eq!(aggs.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["c", "MAX(page_count)", "COUNT(*)"]);
        assert_eq!(by[0].name, "author");
        assert!(parse("SELECT * FROM t ORDER BY name").is_ok());
        assert!(parse("SELECT FROM").is_err());
        assert!(matches!(parse("SHOW TABLES").unwrap(), Stmt::ShowTables(None)));
    }

    #[test]
    fn show_functions_like_is_sql_like() {
        // `_` is any one character: Elasticsearch lists DAYNAME for 'DAY_%' too.
        assert!(sql_like("DAY_%", "DAY_OF_WEEK"));
        assert!(sql_like("DAY_%", "DAYNAME"));
        assert!(!sql_like("DAY_%", "DAY"));
        assert!(sql_like("DAY%", "DAYNAME"));
        assert!(!sql_like("A*", "AVG"));
        assert!(sql_like("A*", "A*"));
        assert!(sql_like("%", ""));
        assert!(sql_like("_VG", "AVG"));
        assert!(!sql_like("_", "AB"));
        assert!(sql_like("%M%X%", "MAX_MIN_X"));
    }

    #[test]
    fn like_patterns_translate() {
        let Stmt::Select { query, .. } = parse("SELECT a FROM t WHERE a LIKE 'x%_y'").unwrap() else { panic!() };
        let Cmd::Where(e) = &query.cmds[1] else { panic!() };
        assert!(matches!(&e.kind, ExprKind::Like(_, p, false, false) if p == "x*?y"));
    }
}
