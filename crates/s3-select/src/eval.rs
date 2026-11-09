//! Expression evaluation: operators with SQL's three-valued logic (MISSING and NULL propagate),
//! LIKE, CAST and the scalar functions.

use crate::ast::*;
use crate::error::{Result, SelectError};
use crate::ts::{DatePart, Timestamp};
use crate::value::{Num, Value, compare, equal, parse_bool, parse_num, strict_equal};

/// AWS's limit on a record (input or output), and so on any value built from one.
pub const MAX_RECORD_BYTES: usize = 1 << 20;

/// A CSV record's fields: `data[ends[i-1]..ends[i]]` is field `i`.
#[derive(Default, Debug)]
pub(crate) struct Fields {
    pub data: Vec<u8>,
    pub ends: Vec<usize>,
}

impl Fields {
    pub fn clear(&mut self) {
        self.data.clear();
        self.ends.clear();
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn get(&self, i: usize) -> Option<&[u8]> {
        let end = *self.ends.get(i)?;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        Some(&self.data[start..end])
    }

    pub fn text(&self, i: usize) -> Result<Option<&str>> {
        self.get(i).map(|b| std::str::from_utf8(b).map_err(|_| SelectError::encoding())).transpose()
    }
}

/// The record an expression is evaluated against.
#[derive(Clone, Copy)]
pub(crate) enum Row<'a> {
    /// No record (the aggregate result).
    None,
    Csv(&'a Fields),
    Json(&'a Value),
}

pub(crate) struct Ctx<'a> {
    pub now: &'a Timestamp,
    pub aggs: &'a [Value],
}

fn data_type(what: impl std::fmt::Display) -> SelectError {
    SelectError::new("InvalidDataType", format!("The SQL expression contains a data type that is not valid: {what}."))
}

fn div_zero() -> SelectError {
    SelectError::new("ExternalEvalException", "The query cannot be evaluated: division by zero.")
}

fn too_long() -> SelectError {
    SelectError::over_max_record()
}

/// MISSING if either is MISSING, else NULL if either is NULL.
fn absent2(a: &Value, b: &Value) -> Option<Value> {
    if matches!(a, Value::Missing) || matches!(b, Value::Missing) {
        Some(Value::Missing)
    } else if a.is_absent() || b.is_absent() {
        Some(Value::Null)
    } else {
        None
    }
}

fn truth(v: &Value, op: &str) -> Result<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null | Value::Missing => Ok(None),
        v => Err(data_type(format_args!("{op} needs BOOL operands, not {}", v.type_name()))),
    }
}

fn num(v: &Value, op: &str) -> Result<Num> {
    match v {
        Value::Int(i) => Ok(Num::Int(*i)),
        Value::Float(f) => Ok(Num::Float(*f)),
        Value::Str(s) => parse_num(s).ok_or_else(|| data_type(format_args!("{op} needs numbers, and {s:?} is not a number"))),
        v => Err(data_type(format_args!("{op} needs numbers, not {}", v.type_name()))),
    }
}

fn as_f64(n: Num) -> f64 {
    match n {
        Num::Int(i) => i as f64,
        Num::Float(f) => f,
    }
}

pub(crate) fn arith(op: ArithOp, a: &Value, b: &Value) -> Result<Value> {
    if let Some(v) = absent2(a, b) {
        return Ok(v);
    }
    let sym = match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Mod => "%",
    };
    match (num(a, sym)?, num(b, sym)?) {
        (Num::Int(x), Num::Int(y)) => {
            let r = match op {
                ArithOp::Add => x.checked_add(y),
                ArithOp::Sub => x.checked_sub(y),
                ArithOp::Mul => x.checked_mul(y),
                ArithOp::Div | ArithOp::Mod if y == 0 => return Err(div_zero()),
                ArithOp::Div => x.checked_div(y),
                ArithOp::Mod => x.checked_rem(y),
            };
            r.map(Value::Int).ok_or_else(SelectError::overflow)
        }
        (x, y) => {
            let (x, y) = (as_f64(x), as_f64(y));
            let r = match op {
                ArithOp::Add => x + y,
                ArithOp::Sub => x - y,
                ArithOp::Mul => x * y,
                ArithOp::Div | ArithOp::Mod if y == 0.0 => return Err(div_zero()),
                ArithOp::Div => x / y,
                ArithOp::Mod => x % y,
            };
            if !r.is_finite() {
                return Err(SelectError::new("ExternalEvalException", "The query cannot be evaluated: numeric overflow."));
            }
            Ok(Value::Float(r))
        }
    }
}

pub(crate) fn cmp(op: CmpOp, a: &Value, b: &Value) -> Value {
    if let Some(v) = absent2(a, b) {
        return v;
    }
    match op {
        CmpOp::Eq => Value::Bool(equal(a, b)),
        CmpOp::Ne => Value::Bool(!equal(a, b)),
        _ => match compare(a, b) {
            None => Value::Null,
            Some(o) => Value::Bool(match op {
                CmpOp::Lt => o.is_lt(),
                CmpOp::Le => o.is_le(),
                CmpOp::Gt => o.is_gt(),
                _ => o.is_ge(),
            }),
        },
    }
}

/// A scalar's text for string operations (`||`, LIKE): strings as they are, numbers, bools and
/// timestamps as CAST(… AS STRING) gives them.
fn scalar_text<'v>(v: &'v Value, op: &str) -> Result<std::borrow::Cow<'v, str>> {
    match v {
        Value::List(_) | Value::Struct(_) => Err(data_type(format_args!("{op} needs strings, not {}", v.type_name()))),
        v => Ok(v.text().unwrap_or_default()),
    }
}

fn want_str<'v>(v: &'v Value, f: &str) -> Result<&'v str> {
    match v {
        Value::Str(s) => Ok(s),
        _ => Err(SelectError::arg_type(f)),
    }
}

fn want_int(v: &Value, f: &str) -> Result<i64> {
    let n = match v {
        Value::Int(i) => return Ok(*i),
        Value::Float(x) => Num::Float(*x),
        Value::Str(s) => parse_num(s).ok_or_else(|| SelectError::arg_type(f))?,
        _ => return Err(SelectError::arg_type(f)),
    };
    match n {
        Num::Int(i) => Ok(i),
        Num::Float(x) if x == x.trunc() && x.abs() < 9.2e18 => Ok(x as i64),
        Num::Float(_) => Err(SelectError::arg_type(f)),
    }
}

fn want_ts(v: &Value, f: &str) -> Result<Timestamp> {
    match v {
        Value::Timestamp(t) => Ok(*t),
        Value::Str(s) => Timestamp::parse(s).ok_or_else(|| SelectError::new("ValueParseFailure", format!("{f}: {s:?} is not a timestamp."))),
        _ => Err(SelectError::arg_type(f)),
    }
}

fn float_to_int(f: f64) -> Result<i64> {
    if f.is_finite() && f.trunc() >= i64::MIN as f64 && f.trunc() < i64::MAX as f64 {
        Ok(f.trunc() as i64)
    } else {
        Err(SelectError::overflow())
    }
}

/// CAST. An empty (or blank) string casts to NULL rather than failing (CSV's empty fields).
pub(crate) fn cast(v: Value, ty: CastType) -> Result<Value> {
    if v.is_absent() {
        return Ok(v);
    }
    let fail = |v: &Value| SelectError::cast_failed(format_args!("{} {} to {ty:?}", v.type_name(), v.text().unwrap_or_default()));
    if let Value::Str(s) = &v
        && ty != CastType::String
        && s.trim().is_empty()
    {
        return Ok(Value::Null);
    }
    Ok(match ty {
        CastType::Int => match &v {
            Value::Int(_) => v,
            Value::Float(f) => Value::Int(float_to_int(*f)?),
            Value::Bool(b) => Value::Int(*b as i64),
            Value::Str(s) => match parse_num(s) {
                Some(Num::Int(i)) => Value::Int(i),
                Some(Num::Float(f)) => Value::Int(float_to_int(f)?),
                None => return Err(fail(&v)),
            },
            _ => return Err(fail(&v)),
        },
        CastType::Float | CastType::Decimal => match &v {
            Value::Int(i) => Value::Float(*i as f64),
            Value::Float(_) => v,
            Value::Bool(b) => Value::Float(if *b { 1.0 } else { 0.0 }),
            Value::Str(s) => match parse_num(s) {
                Some(n) => Value::Float(as_f64(n)),
                None => return Err(fail(&v)),
            },
            _ => return Err(fail(&v)),
        },
        CastType::String => match v {
            Value::Str(_) => v,
            v => Value::Str(v.text().map(|t| t.into_owned()).unwrap_or_default()),
        },
        CastType::Bool => match &v {
            Value::Bool(_) => v,
            Value::Int(i) => Value::Bool(*i != 0),
            Value::Float(f) => Value::Bool(*f != 0.0),
            Value::Str(s) => match (parse_bool(s), parse_num(s)) {
                (Some(b), _) => Value::Bool(b),
                (None, Some(n)) => Value::Bool(as_f64(n) != 0.0),
                _ => return Err(fail(&v)),
            },
            _ => return Err(fail(&v)),
        },
        CastType::Timestamp => match &v {
            Value::Timestamp(_) => v,
            Value::Str(s) => Value::Timestamp(Timestamp::parse(s).ok_or_else(|| fail(&v))?),
            _ => return Err(fail(&v)),
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum LikeTok {
    Char(char),
    One,
    Any,
}

fn compile_like(p: &str, esc: Option<char>) -> Result<Vec<LikeTok>> {
    let mut out = Vec::with_capacity(p.len());
    let mut it = p.chars();
    while let Some(c) = it.next() {
        if Some(c) == esc {
            match it.next() {
                Some(n) => out.push(LikeTok::Char(n)),
                None => {
                    return Err(SelectError::new(
                        "EvaluatorLikePatternInvalidEscapeSequence",
                        "The LIKE pattern ends with its escape character.",
                    ));
                }
            }
        } else if c == '%' {
            if out.last() != Some(&LikeTok::Any) {
                out.push(LikeTok::Any);
            }
        } else if c == '_' {
            out.push(LikeTok::One);
        } else {
            out.push(LikeTok::Char(c));
        }
    }
    Ok(out)
}

/// Work limit for one LIKE with `_` wildcards (the general matcher is O(text × pattern) at worst).
pub const MAX_LIKE_STEPS: u64 = 50_000_000;

/// LIKE matching. Patterns of literals and `%` only (the common case) match in linear time:
/// each literal run is found leftmost in turn, the first anchored at the start and the last at
/// the end. Others backtrack to the last `%`, within [`MAX_LIKE_STEPS`].
fn like_match(toks: &[LikeTok], s: &str) -> Result<bool> {
    if !toks.contains(&LikeTok::One) {
        let mut segs: Vec<String> = Vec::new();
        let mut cur = String::new();
        for t in toks {
            match t {
                LikeTok::Char(c) => cur.push(*c),
                _ => segs.push(std::mem::take(&mut cur)),
            }
        }
        segs.push(cur);
        // segs[0] is before the first %, segs[last] after the last; with no %, one segment.
        if segs.len() == 1 {
            return Ok(s == segs[0]);
        }
        let (first, last) = (&segs[0], &segs[segs.len() - 1]);
        if s.len() < first.len() + last.len() || !s.starts_with(first.as_str()) || !s.ends_with(last.as_str()) {
            return Ok(false);
        }
        let mut hay = &s.as_bytes()[first.len()..s.len() - last.len()];
        for seg in &segs[1..segs.len() - 1] {
            match memchr::memmem::find(hay, seg.as_bytes()) {
                Some(i) => hay = &hay[i + seg.len()..],
                None => return Ok(false),
            }
        }
        return Ok(true);
    }
    let s: Vec<char> = s.chars().collect();
    let (mut si, mut pi) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    let mut steps = 0u64;
    while si < s.len() {
        steps += 1;
        if steps > MAX_LIKE_STEPS {
            return Err(SelectError::new("LikeInvalidInputs", "The LIKE pattern is too complex for this value."));
        }
        match toks.get(pi) {
            Some(LikeTok::Char(c)) if *c == s[si] => {
                si += 1;
                pi += 1;
            }
            Some(LikeTok::One) => {
                si += 1;
                pi += 1;
            }
            Some(LikeTok::Any) => {
                pi += 1;
                star = Some((pi, si));
            }
            _ => match star {
                Some((sp, ss)) => {
                    pi = sp;
                    si = ss + 1;
                    star = Some((sp, ss + 1));
                }
                None => return Ok(false),
            },
        }
    }
    Ok(toks[pi..].iter().all(|t| *t == LikeTok::Any))
}

pub(crate) fn like(s: &str, pattern: &str, escape: Option<&str>) -> Result<bool> {
    let esc = match escape {
        None => None,
        Some(e) => {
            let mut cs = e.chars();
            match (cs.next(), cs.next()) {
                (Some(c), None) => Some(c),
                _ => {
                    return Err(SelectError::new(
                        "EvaluatorLikePatternInvalidEscapeSequence",
                        "The LIKE escape must be a single character.",
                    ));
                }
            }
        }
    };
    like_match(&compile_like(pattern, esc)?, s)
}

fn find_field<'v>(fields: &'v [(String, Value)], name: &Name) -> Result<Option<&'v Value>> {
    if name.quoted {
        return Ok(fields.iter().find(|(k, _)| *k == name.text).map(|(_, v)| v));
    }
    let mut found = None;
    for (k, v) in fields {
        if k.eq_ignore_ascii_case(&name.text) {
            if found.is_some() {
                return Err(SelectError::new(
                    "AmbiguousFieldName",
                    format!("The field name {} matches multiple fields in the record. Double-quote it to match its case.", name.text),
                ));
            }
            found = Some(v);
        }
    }
    Ok(found)
}

/// Follow a path; `None` is MISSING.
pub(crate) fn walk<'v>(v: &'v Value, steps: &[Step]) -> Result<Option<&'v Value>> {
    let mut cur = v;
    for s in steps {
        cur = match (s, cur) {
            (Step::Name(n), Value::Struct(fields)) => match find_field(fields, n)? {
                Some(x) => x,
                None => return Ok(None),
            },
            (Step::Index(i), Value::List(items)) => match items.get(*i) {
                Some(x) => x,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
    Ok(Some(cur))
}

/// JSON `_1`: the row's `_1` attribute if it has one, else the row itself.
fn row_or_1(v: &Value) -> &Value {
    if let Value::Struct(fields) = v
        && let Some((_, x)) = fields.iter().find(|(k, _)| k == "_1")
    {
        return x;
    }
    v
}

fn trim(s: &str, mode: TrimMode, chars: &str) -> String {
    let mut sorted: Vec<char> = chars.chars().collect();
    sorted.sort_unstable();
    sorted.dedup();
    let set = |c: char| sorted.binary_search(&c).is_ok();
    match mode {
        TrimMode::Leading => s.trim_start_matches(set),
        TrimMode::Trailing => s.trim_end_matches(set),
        TrimMode::Both => s.trim_matches(set),
    }
    .to_string()
}

/// SUBSTRING with 1-based positions: the characters at positions `[start, start + len)` that
/// exist (a start below 1 shortens the result, as in SQL).
fn substring(s: &str, start: i64, len: Option<i64>) -> Result<String> {
    let end = match len {
        Some(l) if l < 0 => return Err(SelectError::illegal_arg("SUBSTRING", "the length is negative")),
        Some(l) => start.saturating_add(l),
        None => i64::MAX,
    };
    let from = start.max(1);
    if end <= from {
        return Ok(String::new());
    }
    let skip = usize::try_from(from - 1).unwrap_or(usize::MAX);
    let take = usize::try_from(end - from).unwrap_or(usize::MAX);
    Ok(s.chars().skip(skip).take(take).collect())
}

/// Evaluate an expression. The recursion goes through this small dispatcher and one small
/// helper per node, so each level of the tree costs little stack (even in debug builds).
pub(crate) fn eval(e: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    match e {
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Col(i) => eval_col(*i, row),
        Expr::Row => Ok(match row {
            Row::Json(v) => v.clone(),
            _ => Value::Missing,
        }),
        Expr::RowOr1 => Ok(match row {
            Row::Json(v) => row_or_1(v).clone(),
            _ => Value::Missing,
        }),
        Expr::Path(base, steps) => eval_path(base, steps, row, cx),
        Expr::Neg(a) => eval_neg(a, row, cx),
        Expr::Not(a) => eval_not(a, row, cx),
        Expr::Arith(op, a, b) => eval_arith(*op, a, b, row, cx),
        Expr::Cmp(op, a, b) => eval_cmp(*op, a, b, row, cx),
        Expr::And(a, b) => eval_and(a, b, row, cx),
        Expr::Or(a, b) => eval_or(a, b, row, cx),
        Expr::Concat(a, b) => eval_concat(a, b, row, cx),
        Expr::Like { expr, pattern, escape, negated } => eval_like(expr, pattern, escape.as_deref(), *negated, row, cx),
        Expr::Between { expr, low, high, negated } => eval_between(expr, low, high, *negated, row, cx),
        Expr::In { expr, list, negated } => eval_in(expr, list, *negated, row, cx),
        Expr::Is { expr, what, negated } => eval_is(expr, *what, *negated, row, cx),
        Expr::Case { operand, whens, otherwise } => eval_case(operand.as_deref(), whens, otherwise.as_deref(), row, cx),
        Expr::Cast(a, ty) => eval_cast(a, *ty, row, cx),
        Expr::Call(f, args) => call(*f, args, row, cx),
        Expr::Trim { mode, chars, expr } => eval_trim(*mode, chars.as_deref(), expr, row, cx),
        Expr::Extract(part, a) => eval_extract(*part, a, row, cx),
        Expr::DateAdd(part, n, t) => eval_date_add(*part, n, t, row, cx),
        Expr::DateDiff(part, a, b) => eval_date_diff(*part, a, b, row, cx),
        Expr::AggRef(i) => Ok(cx.aggs.get(*i).cloned().unwrap_or(Value::Null)),
        Expr::Ident(_) | Expr::Agg(..) => Err(SelectError::internal("an unbound expression was evaluated")),
    }
}

#[inline(never)]
fn eval_col(i: usize, row: Row<'_>) -> Result<Value> {
    Ok(match row {
        Row::Csv(f) => match f.text(i)? {
            Some(s) => Value::Str(s.to_string()),
            None => Value::Missing,
        },
        _ => Value::Missing,
    })
}

#[inline(never)]
fn eval_path(base: &Expr, steps: &[Step], row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    Ok(match (base, row) {
        (Expr::Row, Row::Json(v)) => walk(v, steps)?.cloned().unwrap_or(Value::Missing),
        (Expr::RowOr1, Row::Json(v)) => walk(row_or_1(v), steps)?.cloned().unwrap_or(Value::Missing),
        _ => {
            let b = eval(base, row, cx)?;
            walk(&b, steps)?.cloned().unwrap_or(Value::Missing)
        }
    })
}

#[inline(never)]
fn eval_neg(a: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    Ok(match eval(a, row, cx)? {
        v if v.is_absent() => v,
        v => match num(&v, "unary -")? {
            Num::Int(i) => Value::Int(i.checked_neg().ok_or_else(SelectError::overflow)?),
            Num::Float(f) => Value::Float(-f),
        },
    })
}

#[inline(never)]
fn eval_not(a: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    Ok(match truth(&eval(a, row, cx)?, "NOT")? {
        Some(b) => Value::Bool(!b),
        None => Value::Null,
    })
}

#[inline(never)]
fn eval_arith(op: ArithOp, a: &Expr, b: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let x = eval(a, row, cx)?;
    let y = eval(b, row, cx)?;
    arith(op, &x, &y)
}

#[inline(never)]
fn eval_cmp(op: CmpOp, a: &Expr, b: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let x = eval(a, row, cx)?;
    let y = eval(b, row, cx)?;
    Ok(cmp(op, &x, &y))
}

#[inline(never)]
fn eval_and(a: &Expr, b: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let l = truth(&eval(a, row, cx)?, "AND")?;
    if l == Some(false) {
        return Ok(Value::Bool(false));
    }
    Ok(match (l, truth(&eval(b, row, cx)?, "AND")?) {
        (_, Some(false)) => Value::Bool(false),
        (Some(true), Some(true)) => Value::Bool(true),
        _ => Value::Null,
    })
}

#[inline(never)]
fn eval_or(a: &Expr, b: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let l = truth(&eval(a, row, cx)?, "OR")?;
    if l == Some(true) {
        return Ok(Value::Bool(true));
    }
    Ok(match (l, truth(&eval(b, row, cx)?, "OR")?) {
        (_, Some(true)) => Value::Bool(true),
        (Some(false), Some(false)) => Value::Bool(false),
        _ => Value::Null,
    })
}

#[inline(never)]
fn eval_concat(a: &Expr, b: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let x = eval(a, row, cx)?;
    let y = eval(b, row, cx)?;
    if let Some(v) = absent2(&x, &y) {
        return Ok(v);
    }
    let (x, y) = (scalar_text(&x, "||")?, scalar_text(&y, "||")?);
    if x.len() + y.len() > MAX_RECORD_BYTES {
        return Err(too_long());
    }
    Ok(Value::Str(format!("{x}{y}")))
}

#[inline(never)]
fn eval_like(expr: &Expr, pattern: &Expr, escape: Option<&Expr>, negated: bool, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let v = eval(expr, row, cx)?;
    let p = eval(pattern, row, cx)?;
    let esc = escape.map(|x| eval(x, row, cx)).transpose()?;
    if let Some(a) = absent2(&v, &p) {
        return Ok(a);
    }
    if let Some(esc) = &esc
        && esc.is_absent()
    {
        return Ok(Value::Null);
    }
    let s = scalar_text(&v, "LIKE")?;
    let p = match &p {
        Value::Str(p) => p.as_str(),
        _ => return Err(SelectError::new("LikeInvalidInputs", "The LIKE pattern must be a string.")),
    };
    let esc = match &esc {
        None => None,
        Some(Value::Str(e)) => Some(e.as_str()),
        Some(_) => return Err(SelectError::new("LikeInvalidInputs", "The LIKE escape must be a string.")),
    };
    Ok(Value::Bool(like(&s, p, esc)? != negated))
}

#[inline(never)]
fn eval_between(expr: &Expr, low: &Expr, high: &Expr, negated: bool, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let v = eval(expr, row, cx)?;
    let lo = eval(low, row, cx)?;
    let hi = eval(high, row, cx)?;
    if let Some(a) = absent2(&v, &lo).or_else(|| absent2(&v, &hi)) {
        return Ok(a);
    }
    Ok(match (compare(&v, &lo), compare(&v, &hi)) {
        (Some(a), Some(b)) => Value::Bool((a.is_ge() && b.is_le()) != negated),
        _ => Value::Null,
    })
}

#[inline(never)]
fn eval_in(expr: &Expr, list: &[Expr], negated: bool, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let v = eval(expr, row, cx)?;
    if v.is_absent() {
        return Ok(Value::Null);
    }
    let mut unknown = false;
    for item in list {
        let x = eval(item, row, cx)?;
        if x.is_absent() {
            unknown = true;
        } else if equal(&v, &x) {
            return Ok(Value::Bool(!negated));
        }
    }
    Ok(if unknown { Value::Null } else { Value::Bool(negated) })
}

#[inline(never)]
fn eval_is(expr: &Expr, what: IsWhat, negated: bool, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let v = eval(expr, row, cx)?;
    let r = match what {
        IsWhat::Null => v.is_absent(),
        IsWhat::Missing => matches!(v, Value::Missing),
        IsWhat::True => v == Value::Bool(true),
        IsWhat::False => v == Value::Bool(false),
    };
    Ok(Value::Bool(r != negated))
}

#[inline(never)]
fn eval_case(operand: Option<&Expr>, whens: &[(Expr, Expr)], otherwise: Option<&Expr>, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let op = operand.map(|o| eval(o, row, cx)).transpose()?;
    for (w, t) in whens {
        let wv = eval(w, row, cx)?;
        let hit = match &op {
            Some(o) => !o.is_absent() && !wv.is_absent() && equal(o, &wv),
            None => truth(&wv, "CASE WHEN")? == Some(true),
        };
        if hit {
            return eval(t, row, cx);
        }
    }
    match otherwise {
        Some(o) => eval(o, row, cx),
        None => Ok(Value::Null),
    }
}

#[inline(never)]
fn eval_cast(a: &Expr, ty: CastType, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    cast(eval(a, row, cx)?, ty)
}

#[inline(never)]
fn eval_trim(mode: TrimMode, chars: Option<&Expr>, expr: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let s = eval(expr, row, cx)?;
    let c = chars.map(|c| eval(c, row, cx)).transpose()?;
    if s.is_absent() {
        return Ok(s);
    }
    let set = match &c {
        None => " ",
        Some(v) if v.is_absent() => return Ok(Value::Null),
        Some(v) => want_str(v, "TRIM")?,
    };
    Ok(Value::Str(trim(want_str(&s, "TRIM")?, mode, set)))
}

#[inline(never)]
fn eval_extract(part: DatePart, a: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    Ok(match eval(a, row, cx)? {
        v if v.is_absent() => v,
        v => Value::Int(want_ts(&v, "EXTRACT")?.extract(part)),
    })
}

#[inline(never)]
fn eval_date_add(part: DatePart, n: &Expr, t: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let n = eval(n, row, cx)?;
    let t = eval(t, row, cx)?;
    if let Some(a) = absent2(&n, &t) {
        return Ok(a);
    }
    Ok(Value::Timestamp(want_ts(&t, "DATE_ADD")?.add(part, want_int(&n, "DATE_ADD")?)?))
}

#[inline(never)]
fn eval_date_diff(part: DatePart, a: &Expr, b: &Expr, row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let a = eval(a, row, cx)?;
    let b = eval(b, row, cx)?;
    if let Some(x) = absent2(&a, &b) {
        return Ok(x);
    }
    Ok(Value::Int(Timestamp::diff(part, &want_ts(&a, "DATE_DIFF")?, &want_ts(&b, "DATE_DIFF")?)?))
}

#[inline(never)]
fn call(f: Func, args: &[Expr], row: Row<'_>, cx: &Ctx<'_>) -> Result<Value> {
    let ev = |x: &Expr| eval(x, row, cx);
    match f {
        Func::Coalesce => {
            for a in args {
                let v = ev(a)?;
                if !v.is_absent() {
                    return Ok(v);
                }
            }
            return Ok(Value::Null);
        }
        Func::NullIf => {
            let (a, b) = (ev(&args[0])?, ev(&args[1])?);
            return Ok(if a.is_absent() || (!b.is_absent() && strict_equal(&a, &b)) { Value::Null } else { a });
        }
        Func::UtcNow => return Ok(Value::Timestamp(*cx.now)),
        _ => {}
    }
    let vals = args.iter().map(ev).collect::<Result<Vec<_>>>()?;
    if vals.iter().any(|v| matches!(v, Value::Missing)) {
        return Ok(Value::Missing);
    }
    if vals.iter().any(Value::is_absent) {
        return Ok(Value::Null);
    }
    Ok(match f {
        Func::Lower => Value::Str(want_str(&vals[0], "LOWER")?.to_lowercase()),
        Func::Upper => Value::Str(want_str(&vals[0], "UPPER")?.to_uppercase()),
        Func::CharLength => Value::Int(want_str(&vals[0], "CHAR_LENGTH")?.chars().count() as i64),
        Func::Substring => {
            let s = want_str(&vals[0], "SUBSTRING")?;
            let start = want_int(&vals[1], "SUBSTRING")?;
            let len = vals.get(2).map(|v| want_int(v, "SUBSTRING")).transpose()?;
            Value::Str(substring(s, start, len)?)
        }
        Func::ToString => {
            let t = want_ts(&vals[0], "TO_STRING")?;
            let p = want_str(&vals[1], "TO_STRING")?;
            Value::Str(t.format(p)?)
        }
        Func::ToTimestamp => Value::Timestamp(want_ts(&vals[0], "TO_TIMESTAMP")?),
        Func::Coalesce | Func::NullIf | Func::UtcNow => unreachable!(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bind::{Schema, bind};
    use crate::parser::parse;

    /// Evaluate the first SELECT item against a CSV record of `fields`.
    fn csv_eval(expr: &str, fields: &[&str]) -> Result<Value> {
        let q = parse(&format!("select {expr} from s3object")).unwrap();
        let b = bind(&q, &Schema::Csv { header: None, quoted_literals: true, alias: None })?;
        let mut f = Fields::default();
        for x in fields {
            f.data.extend_from_slice(x.as_bytes());
            f.ends.push(f.data.len());
        }
        let now = Timestamp::parse("2020-01-02T03:04:05.678Z").unwrap();
        eval(&b.columns[0].1, Row::Csv(&f), &Ctx { now: &now, aggs: &[] })
    }

    fn v(expr: &str) -> Value {
        csv_eval(expr, &["10", "abc", "", "2.5"]).unwrap_or_else(|e| panic!("{expr}: {e}"))
    }

    fn err(expr: &str) -> &'static str {
        csv_eval(expr, &["10", "abc", "", "2.5"]).unwrap_err().code
    }

    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }

    #[test]
    fn arithmetic() {
        assert_eq!(v("1 + 2 * 3"), Value::Int(7));
        assert_eq!(v("7 / 2"), Value::Int(3));
        assert_eq!(v("-7 % 3"), Value::Int(-1));
        assert_eq!(v("7.0 / 2"), Value::Float(3.5));
        assert_eq!(v("_1 + 1"), Value::Int(11));
        assert_eq!(v("_4 * 2"), Value::Float(5.0));
        assert_eq!(v("-_1"), Value::Int(-10));
        assert_eq!(v("1 + null"), Value::Null);
        assert_eq!(v("1 + missing"), Value::Missing);
        assert_eq!(v("_9 + 1"), Value::Missing);
        assert_eq!(err("1 / 0"), "ExternalEvalException");
        assert_eq!(err("1.5 % 0"), "ExternalEvalException");
        assert_eq!(err("9223372036854775807 + 1"), "IntegerOverflow");
        assert_eq!(err("_2 + 1"), "InvalidDataType");
        assert_eq!(err("true + 1"), "InvalidDataType");
    }

    #[test]
    fn comparisons_and_logic() {
        assert_eq!(v("_1 = 10"), Value::Bool(true));
        assert_eq!(v("_1 > 9"), Value::Bool(true));
        assert_eq!(v("_1 > '9'"), Value::Bool(false)); // string vs string
        assert_eq!(v("_1 <> 10.0"), Value::Bool(false));
        assert_eq!(v("_2 = 10"), Value::Bool(false));
        assert_eq!(v("_2 < 10"), Value::Null);
        assert_eq!(v("null = null"), Value::Null);
        assert_eq!(v("_9 = 1"), Value::Missing);
        assert_eq!(v("true and null"), Value::Null);
        assert_eq!(v("false and null"), Value::Bool(false));
        assert_eq!(v("true or null"), Value::Bool(true));
        assert_eq!(v("false or null"), Value::Null);
        assert_eq!(v("not null"), Value::Null);
        assert_eq!(v("not (1 = 2)"), Value::Bool(true));
        assert_eq!(err("_1 and true"), "InvalidDataType");
        assert_eq!(v("_1 between 5 and 10"), Value::Bool(true));
        assert_eq!(v("_1 not between 11 and 20"), Value::Bool(true));
        assert_eq!(v("_1 between null and 20"), Value::Null);
        assert_eq!(v("_1 in (1, 10)"), Value::Bool(true));
        assert_eq!(v("_1 in (1, null)"), Value::Null);
        assert_eq!(v("_1 not in (1, 2)"), Value::Bool(true));
        assert_eq!(v("_2 in ('abc')"), Value::Bool(true));
        assert_eq!(v("_3 is null"), Value::Bool(false));
        assert_eq!(v("_9 is null"), Value::Bool(true));
        assert_eq!(v("_9 is missing"), Value::Bool(true));
        assert_eq!(v("null is not missing"), Value::Bool(true));
        assert_eq!(v("(1 = 1) is true"), Value::Bool(true));
        assert_eq!(v("(1 = 1) = true"), Value::Bool(true));
        assert_eq!(v("`2010-01-01T` < `2010-01-02T`"), Value::Bool(true));
        assert_eq!(v("`2010-01-01T00:00Z` = '2010-01-01T'"), Value::Bool(true));
    }

    #[test]
    fn like_patterns() {
        assert!(like("abc", "a%", None).unwrap());
        assert!(like("abc", "_b_", None).unwrap());
        assert!(like("abc", "%c", None).unwrap());
        assert!(like("abc", "%", None).unwrap());
        assert!(!like("abc", "a_", None).unwrap());
        assert!(like("", "%", None).unwrap());
        assert!(!like("", "_", None).unwrap());
        assert!(like("a%c", "a!%c", Some("!")).unwrap());
        assert!(!like("abc", "a!%c", Some("!")).unwrap());
        assert!(like("_ar", "%_ar", Some("%")).unwrap());
        assert!(!like("x_ar", "%_ar", Some("%")).unwrap());
        assert!(like("aeio_", "%aeio$_", Some("$")).unwrap());
        assert!(like("mississippi", "%iss%ppi", None).unwrap());
        assert!(like("héllo", "h_llo", None).unwrap());
        assert!(like("y[y-z]", "%y[y-z]", None).unwrap());
        assert!(!like("yz", "%y[y-z]", None).unwrap());
        assert_eq!(like("a", "a!", Some("!")).unwrap_err().code, "EvaluatorLikePatternInvalidEscapeSequence");
        assert_eq!(like("a", "a", Some("!!")).unwrap_err().code, "EvaluatorLikePatternInvalidEscapeSequence");
        assert_eq!(v("_2 like 'a%'"), Value::Bool(true));
        assert_eq!(v("_2 not like 'a%'"), Value::Bool(false));
        assert_eq!(v("_1 like '1_'"), Value::Bool(true));
        assert_eq!(v("_9 like 'a'"), Value::Missing);
        assert_eq!(v("_2 like \"%b%\""), Value::Bool(true));
        // %-only patterns are linear however long; others are bounded.
        let long = "a".repeat(MAX_RECORD_BYTES);
        let pattern = format!("%{}b%", "a".repeat(100_000));
        assert!(!like(&long, &pattern, None).unwrap());
        assert!(like(&long, &format!("a%{}%a", "a".repeat(100_000)), None).unwrap());
        assert!(!like("ab", "a%b%b", None).unwrap());
        assert!(like("abb", "a%b%b", None).unwrap());
        assert!(!like("a", "a%a", None).unwrap());
        assert!(like("", "", None).unwrap());
        assert!(!like("x", "", None).unwrap());
        let pattern = format!("%{}b", "_a".repeat(2000));
        assert_eq!(like(&long[..200_000], &pattern, None).unwrap_err().code, "LikeInvalidInputs");
        assert!(like("xayaz", "%_a_a%", None).unwrap());
    }

    #[test]
    fn casts() {
        assert_eq!(v("cast(_1 as int)"), Value::Int(10));
        assert_eq!(v("cast(_4 as int)"), Value::Int(2));
        assert_eq!(v("cast(_4 as float)"), Value::Float(2.5));
        assert_eq!(v("cast(_4 as decimal(5,2))"), Value::Float(2.5));
        assert_eq!(v("cast(_3 as int)"), Value::Null);
        assert_eq!(err("cast(_2 as int)"), "CastFailed");
        assert_eq!(v("cast(1 as string)"), s("1"));
        assert_eq!(v("cast(1.5 as string)"), s("1.5"));
        assert_eq!(v("cast(true as string)"), s("true"));
        assert_eq!(v("cast(0 as bool)"), Value::Bool(false));
        assert_eq!(v("cast(int(_1) as bool)"), Value::Bool(true));
        assert_eq!(v("cast('TRUE' as bool)"), Value::Bool(true));
        assert_eq!(err("cast('maybe' as bool)"), "CastFailed");
        assert_eq!(v("cast('2007-04-05T14:30Z' as timestamp)"), Value::Timestamp(Timestamp::parse("2007-04-05T14:30Z").unwrap()));
        assert_eq!(err("cast('x' as timestamp)"), "CastFailed");
        assert_eq!(err("cast(1e30 as int)"), "IntegerOverflow");
        assert_eq!(v("cast(null as int)"), Value::Null);
        assert_eq!(v("cast(missing as int)"), Value::Missing);
        assert_eq!(v("float(_1)"), Value::Float(10.0));
        assert_eq!(v("cast(`2010T` as string)"), s("2010T"));
        assert_eq!(v("cast('+00' as int)"), Value::Int(0));
    }

    #[test]
    fn string_functions() {
        assert_eq!(v("lower('AbCdEfG!@#$')"), s("abcdefg!@#$"));
        assert_eq!(v("upper('AbCdEfG!@#$')"), s("ABCDEFG!@#$"));
        assert_eq!(v("char_length('')"), Value::Int(0));
        assert_eq!(v("character_length('abcdefg')"), Value::Int(7));
        assert_eq!(v("char_length('héllo')"), Value::Int(5));
        assert_eq!(v("substring('123456789', 0)"), s("123456789"));
        assert_eq!(v("substring('123456789', 1)"), s("123456789"));
        assert_eq!(v("substring('123456789', 2)"), s("23456789"));
        assert_eq!(v("substring('123456789', -4)"), s("123456789"));
        assert_eq!(v("substring('123456789', 0, 999)"), s("123456789"));
        assert_eq!(v("substring('123456789', 1, 5)"), s("12345"));
        assert_eq!(v("substring('123456789', 0, 3)"), s("12"));
        assert_eq!(v("substring('123456789', -5, 3)"), s(""));
        assert_eq!(v("substring('123456789' from 4 for 2)"), s("45"));
        assert_eq!(v("substring(_1, 2, 1)"), s("0"));
        assert_eq!(err("substring('abc', 1, -1)"), "IllegalSqlFunctionArgument");
        assert_eq!(v("trim('  foobar  ')"), s("foobar"));
        assert_eq!(v("trim(' \tfoobar\t ')"), s("\tfoobar\t"));
        assert_eq!(v("trim(leading from '  foobar  ')"), s("foobar  "));
        assert_eq!(v("trim(trailing from '  foobar  ')"), s("  foobar"));
        assert_eq!(v("trim(both from '  foobar  ')"), s("foobar"));
        assert_eq!(v("trim(both '12' from '1112211foobar22211122')"), s("foobar"));
        assert_eq!(v("'a' || 'b' || 1"), s("ab1"));
        assert_eq!(v("'a' || null"), Value::Null);
        assert_eq!(err("lower(1)"), "IncorrectSqlFunctionArgumentType");
        assert_eq!(v("lower(null)"), Value::Null);
        assert_eq!(v("upper(_9)"), Value::Missing);
    }

    #[test]
    fn conditional_functions() {
        assert_eq!(v("coalesce(1)"), Value::Int(1));
        assert_eq!(v("coalesce(null)"), Value::Null);
        assert_eq!(v("coalesce(missing, missing)"), Value::Null);
        assert_eq!(v("coalesce(null, null, 1)"), Value::Int(1));
        assert_eq!(v("coalesce(missing, 'x')"), s("x"));
        assert_eq!(v("nullif(1, 1)"), Value::Null);
        assert_eq!(v("nullif(1, 2)"), Value::Int(1));
        assert_eq!(v("nullif(1.0, 1)"), Value::Null);
        assert_eq!(v("nullif(1, '1')"), Value::Int(1));
        assert_eq!(v("nullif(1, null)"), Value::Int(1));
        assert_eq!(v("nullif(null, 1)"), Value::Null);
        assert_eq!(v("nullif(missing, null)"), Value::Null);
        assert_eq!(v("nullif(_1, _1)"), Value::Null);
        assert_eq!(v("case when _1 = 10 then 'ten' else 'other' end"), s("ten"));
        assert_eq!(v("case _2 when 'x' then 1 when 'abc' then 2 end"), Value::Int(2));
        assert_eq!(v("case _2 when 'x' then 1 end"), Value::Null);
        assert_eq!(v("case when null then 1 else 2 end"), Value::Int(2));
    }

    #[test]
    fn date_functions() {
        let ts = |x: &str| Value::Timestamp(Timestamp::parse(x).unwrap());
        assert_eq!(v("utcnow()"), ts("2020-01-02T03:04:05.678Z"));
        assert_eq!(v("date_add(year, 5, `2010-01-01T`)"), ts("2015-01-01"));
        assert_eq!(v("date_add(day, '1', '2010-01-01')"), ts("2010-01-02"));
        assert_eq!(v("date_diff(day, `2010-01-01T23:00Z`, `2010-01-02T01:00Z`)"), Value::Int(0));
        assert_eq!(v("date_diff(hour, utcnow(), date_add(day, 1, utcnow()))"), Value::Int(24));
        assert_eq!(v("extract(year from to_timestamp('2010-01-01T'))"), Value::Int(2010));
        assert_eq!(v("extract(month from '2010-10T')"), Value::Int(10));
        assert_eq!(v("to_string(`1969-07-20T20:18Z`, 'MMMM d, y')"), s("July 20, 1969"));
        assert_eq!(v("cast(to_string(to_timestamp('19500101T000000Z'), 'x') as int)"), Value::Int(0));
        assert_eq!(v("to_timestamp('2007-02-23T12:14:33.079-08:00')"), ts("2007-02-23T12:14:33.079-08:00"));
        assert_eq!(err("to_timestamp('nope')"), "ValueParseFailure");
        assert_eq!(err("extract(year from 1)"), "IncorrectSqlFunctionArgumentType");
        assert_eq!(v("extract(year from null)"), Value::Null);
    }

    #[test]
    fn json_paths() {
        let row = crate::value::parse_json(br#"{"a": {"B": [10, {"c": 3}]}, "x y": 1, "dup": 1, "DUP": 2, "_1": "one"}"#).unwrap();
        let q = parse("select s.a.b[1].c, s.\"x y\", s.a.b[5], s.dup, s.\"DUP\", _1, s.a.B[0] from s3object[*] s").unwrap();
        let b = bind(&q, &Schema::Json { aliases: vec![Name { text: "s".into(), quoted: false }] }).unwrap();
        let now = Timestamp::now();
        let cx = Ctx { now: &now, aggs: &[] };
        let r = |i: usize| eval(&b.columns[i].1, Row::Json(&row), &cx);
        assert_eq!(r(0).unwrap(), Value::Int(3));
        assert_eq!(r(1).unwrap(), Value::Int(1));
        assert_eq!(r(2).unwrap(), Value::Missing);
        assert_eq!(r(3).unwrap_err().code, "AmbiguousFieldName");
        assert_eq!(r(4).unwrap(), Value::Int(2));
        assert_eq!(r(5).unwrap(), s("one"));
        assert_eq!(r(6).unwrap(), Value::Int(10));
    }
}
