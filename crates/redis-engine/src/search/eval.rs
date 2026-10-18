//! Evaluation of the expression language (`aggregate/expr/expression.c`) and its function
//! library (`aggregate/functions`), as used by index `FILTER` rules, `APPLY`/`FILTER` steps
//! and query-time expressions.
//!
//! Values follow the module's `RSValue` model: hash fields arrive as Redis strings (which
//! some functions reject where they demand a plain string), comparisons coerce numeric
//! strings, and numbers print as integers or `%.12g`. Errors go to a `QueryError` that keeps
//! the first error set; the evaluation's own status (`Rc`) is separate, so an evaluation can
//! succeed while an error stays pending (`@missing && 1`), and `exists()` clears it.
//!
//! The date functions use musl's `gmtime_r`, `strftime` and `strptime` (the reference server
//! runs on Alpine), and double-to-integer casts saturate as they do on aarch64.

use super::error::{Code, QErr};
use super::expr::{Cond, Expr, FUNCTIONS, Literal, num_to_str};
use crate::util::cstr;
use redis_proto::Reply;
use std::cmp::Ordering;

// ---------------------------------------------------------------------------------------
// Values (`RSValue`).

/// A value (`RSValue`). References are resolved, so a value is what a reference points to.
#[derive(Clone, Debug, PartialEq)]
pub enum RsValue {
    Null,
    Number(f64),
    /// `RSValueType_String`: literals, function results, sorting-vector and JSON strings.
    String(Vec<u8>),
    /// `RSValueType_RedisString`: hash fields loaded from the document, serialized JSON.
    RedisString(Vec<u8>),
    Array(Vec<RsValue>),
    Map(Vec<(RsValue, RsValue)>),
    /// A JSON multi-value: left (used by comparisons and functions), middle (the reply's
    /// serialized value), right (the expanded value).
    Trio(Box<[RsValue; 3]>),
}

/// Why `RsValue::compare` could not order two values (`CompareError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpError {
    NaNFloat,
    NoNumberToStringFallback,
    MapComparison,
    IncompatibleAgainstString(Ordering),
    IncompatibleTypes,
}

/// `str_to_float`: Rust's float grammar (`inf`, `NaN`, `1e5`, `.5`, `5.`), no whitespace.
pub fn parse_number(b: &[u8]) -> Option<f64> {
    std::str::from_utf8(b).ok()?.parse::<f64>().ok()
}

impl RsValue {
    pub fn string(b: impl Into<Vec<u8>>) -> Self {
        RsValue::String(b.into())
    }

    /// `RSValue_Type` (after `RSValue_Dereference`), as the numbers errors print.
    pub fn type_code(&self) -> u8 {
        match self {
            RsValue::Number(_) => 1,
            RsValue::String(_) => 2,
            RsValue::Null => 3,
            RsValue::RedisString(_) => 4,
            RsValue::Array(_) => 5,
            RsValue::Trio(_) => 7,
            RsValue::Map(_) => 8,
        }
    }

    /// `RSValue_DereferenceRefAndTrio`: a trio's left value, recursively.
    pub fn leaf(&self) -> &RsValue {
        match self {
            RsValue::Trio(t) => t[0].leaf(),
            v => v,
        }
    }

    /// `RSValue_IsString`: either string type, not through a trio.
    pub fn is_string(&self) -> bool {
        matches!(self, RsValue::String(_) | RsValue::RedisString(_))
    }

    /// `RSValue_StringPtrLen`: the string bytes, through trios.
    pub fn str_bytes(&self) -> Option<&[u8]> {
        match self.leaf() {
            RsValue::String(s) | RsValue::RedisString(s) => Some(s),
            _ => None,
        }
    }

    /// `RSValue_ToNumber`: numbers and numeric strings, through trios.
    pub fn to_number(&self) -> Option<f64> {
        match self.leaf() {
            RsValue::Number(n) => Some(*n),
            RsValue::String(s) | RsValue::RedisString(s) => parse_number(s),
            _ => None,
        }
    }

    /// `RSValue_BoolTest`: non-zero numbers (NaN included), non-empty strings and arrays.
    /// Trios, maps and null are false.
    pub fn bool_test(&self) -> bool {
        match self {
            RsValue::Number(n) => *n != 0.0,
            RsValue::Array(a) => !a.is_empty(),
            RsValue::String(s) | RsValue::RedisString(s) => !s.is_empty(),
            _ => false,
        }
    }

    /// `compare`: the ordering of two values, `fallback` comparing an unconvertible string
    /// with a number's text instead of failing.
    pub fn compare(&self, other: &RsValue, fallback: bool) -> Result<Ordering, CmpError> {
        use RsValue::*;
        let incompatible = |v: &RsValue| matches!(v, Trio(_) | Array(_) | Map(_));
        match (self, other) {
            (Null, Null) => Ok(Ordering::Equal),
            (Null, _) => Ok(Ordering::Less),
            (_, Null) => Ok(Ordering::Greater),
            (Number(a), Number(b)) => a.partial_cmp(b).ok_or(CmpError::NaNFloat),
            (String(a) | RedisString(a), String(b) | RedisString(b)) => Ok(a.as_slice().cmp(b.as_slice())),
            (Number(n), String(s) | RedisString(s)) => num_vs_str(*n, s, fallback),
            (String(s) | RedisString(s), Number(n)) => num_vs_str(*n, s, fallback).map(Ordering::reverse),
            (Array(a), Array(b)) => match (a.first(), b.first()) {
                (Some(x), Some(y)) => x.compare(y, fallback),
                _ => Ok(a.len().cmp(&b.len())),
            },
            (Map(_), Map(_)) => Err(CmpError::MapComparison),
            (Trio(a), Trio(b)) => a[0].compare(&b[0], fallback),
            (Number(n), Trio(t)) => match t[0].to_number() {
                Some(m) => n.partial_cmp(&m).ok_or(CmpError::NaNFloat),
                None => unconvertible(fallback),
            },
            (Trio(t), Number(n)) => match t[0].to_number() {
                Some(m) => m.partial_cmp(n).ok_or(CmpError::NaNFloat),
                None => unconvertible(fallback).map(Ordering::reverse),
            },
            (Number(_), _) => unconvertible(fallback),
            (_, Number(_)) => unconvertible(fallback).map(Ordering::reverse),
            (String(s) | RedisString(s), b) if incompatible(b) => Err(CmpError::IncompatibleAgainstString(s.as_slice().cmp(b""))),
            (a, String(s) | RedisString(s)) if incompatible(a) => Err(CmpError::IncompatibleAgainstString(b"".as_slice().cmp(s))),
            _ => Err(CmpError::IncompatibleTypes),
        }
    }

    /// `RSValue_Cmp`: with no error sink an unconvertible string compares as text against the
    /// number; with one, the comparison is equal and "Error converting string" is set.
    pub fn cmp_with(&self, other: &RsValue, err: Option<&mut Option<QErr>>) -> Ordering {
        if let (RsValue::String(a), RsValue::String(b)) = (self, other) {
            return a.cmp(b);
        }
        let fallback = err.is_none();
        match self.compare(other, fallback) {
            Ok(o) => o,
            Err(CmpError::IncompatibleAgainstString(o)) => o,
            Err(CmpError::NoNumberToStringFallback) => {
                if let Some(e) = err
                    && e.is_none()
                {
                    let msg = "Error converting string".to_string();
                    *e = Some(QErr { code: Code::NumericValueInvalid, public: Some(msg.clone()), private: Some(msg) });
                }
                Ordering::Equal
            }
            Err(_) => Ordering::Equal,
        }
    }

    /// `RSValue_Equal`: NaNs, maps and incompatible pairs are equal; a number never equals
    /// an unconvertible string.
    pub fn equal(&self, other: &RsValue) -> bool {
        match self.compare(other, false) {
            Ok(o) => o == Ordering::Equal,
            Err(CmpError::IncompatibleAgainstString(o)) => o == Ordering::Equal,
            Err(CmpError::NoNumberToStringFallback) => false,
            Err(_) => true,
        }
    }

    /// `RedisModule_Reply_RSValue`. By default (neither flag) numbers are bulk strings in
    /// both protocols. `typed` (internal requests) sends doubles in RESP3 and, in RESP2, the
    /// number's text as an error reply (as the reference does). `expand` (JSON `FORMAT
    /// EXPAND`) sends integral numbers as integers and others as doubles. A trio replies
    /// with its middle value.
    pub fn reply(&self, resp3: bool, typed: bool, expand: bool) -> Reply {
        match self {
            RsValue::String(s) | RsValue::RedisString(s) => Reply::bulk(s),
            RsValue::Number(n) if expand => {
                let ll = *n as i64;
                if ll as f64 == *n { Reply::Int(ll) } else { Reply::Double(*n) }
            }
            RsValue::Number(n) if typed => {
                if resp3 {
                    Reply::Double(*n)
                } else {
                    Reply::Error(num_to_str(*n))
                }
            }
            RsValue::Number(n) => Reply::bulk_str(num_to_str(*n)),
            RsValue::Null => Reply::Nil,
            RsValue::Trio(t) => t[1].reply(resp3, typed, expand),
            RsValue::Array(a) => Reply::Array(a.iter().map(|v| v.reply(resp3, typed, expand)).collect()),
            RsValue::Map(m) => Reply::Map(m.iter().map(|(k, v)| (k.reply(resp3, typed, expand), v.reply(resp3, typed, expand))).collect()),
        }
    }
}

fn num_vs_str(n: f64, s: &[u8], fallback: bool) -> Result<Ordering, CmpError> {
    if let Some(m) = parse_number(s) {
        n.partial_cmp(&m).ok_or(CmpError::NaNFloat)
    } else if fallback {
        Ok(num_to_str(n).as_bytes().cmp(s))
    } else {
        Err(CmpError::NoNumberToStringFallback)
    }
}

fn unconvertible(fallback: bool) -> Result<Ordering, CmpError> {
    if fallback { Ok(Ordering::Greater) } else { Err(CmpError::NoNumberToStringFallback) }
}

// ---------------------------------------------------------------------------------------
// The evaluator (`ExprEval`).

/// `EvalMode`: queries report missing properties; index filters treat a comparison with a
/// missing property as false.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Query,
    Index,
}

/// `EXPR_EVAL_ERR` / `EXPR_EVAL_OK` / `EXPR_EVAL_MISSING`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rc {
    Err,
    Ok,
    Missing,
}

/// Property values by name (`RLookupRow_Get`).
pub trait Lookup {
    /// The property's value; `None` when the row has no value for it.
    fn get(&self, name: &[u8]) -> Option<RsValue>;

    /// `IndexResult_GetMatchedTerms`: up to `max` query terms that matched the current
    /// result. Empty when there is no index result (index filters, rows without one).
    fn matched_terms(&self, _max: usize) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

const ARG: &str = "expr->func.args->args[ii]";

/// `ExprAST_GetLookupKeys`'s error for a NULL child (the parser keeps them).
fn missing_child(label: &str) -> QErr {
    QErr::user(Code::Expr, "Missing (or badly formatted) value for", format!(" {label}"))
}

/// `ExprAST_GetLookupKeys`: every property must name a known key (`has_key`), and no child
/// may be missing. Runs before any evaluation (plan time for queries, every index filter).
pub fn lookup_keys(expr: &Expr, has_key: &dyn Fn(&[u8]) -> bool) -> Result<(), QErr> {
    let sub = |e: Option<&Expr>, label: &str| match e {
        None => Err(missing_child(label)),
        Some(e) => lookup_keys(e, has_key),
    };
    match expr {
        Expr::Property(p) => {
            if !has_key(p) {
                return Err(QErr::user(Code::NoPropKey, "Property not loaded nor in pipeline", format!(": `{}`", redis_proto::escape(p))));
            }
        }
        Expr::Func { args, .. } => {
            for a in args {
                sub(a.as_ref(), ARG)?;
            }
        }
        Expr::Op { left, right, .. } => {
            sub(left.as_deref(), "expr->op.left")?;
            sub(right.as_deref(), "expr->op.right")?;
        }
        Expr::Pred { left, right, .. } => {
            sub(left.as_deref(), "expr->pred.left")?;
            sub(right.as_deref(), "expr->pred.right")?;
        }
        Expr::Inverted(c) => sub(c.as_deref(), "expr->inverted.child")?,
        Expr::Literal(_) => {}
    }
    Ok(())
}

/// The evaluator's state: its mode and the `QueryError` it writes to. In a query pipeline
/// that error belongs to the pipeline and outlives a row: keep one state per pipeline and
/// check `err` after successful rows too (the reference fails the query on it later).
#[derive(Clone, Debug)]
pub struct EvalState {
    pub mode: Mode,
    pub err: Option<QErr>,
}

/// One evaluation with a fresh error: the value, or the error. A query evaluation that
/// succeeds with an error pending (`@missing && 1`) fails, as a single row does in the
/// reference. Evaluations that fail without an error (index mode) report
/// `SEARCH_GENERIC`.
pub fn eval(expr: &Expr, lk: &dyn Lookup, mode: Mode) -> Result<RsValue, QErr> {
    lookup_keys(expr, &|_| true)?;
    let mut st = EvalState::new(mode);
    let v = st.eval_row(expr, lk)?;
    match st.err {
        Some(e) if mode == Mode::Query => Err(e),
        _ => Ok(v),
    }
}

/// `SchemaRule_FilterPasses`: an index-mode evaluation that succeeds with a true value.
/// Any error (bad types, missing children) fails the filter.
pub fn filter_passes(expr: &Expr, lk: &dyn Lookup) -> bool {
    if lookup_keys(expr, &|_| true).is_err() {
        return false;
    }
    let (rc, v) = EvalState::new(Mode::Index).run(expr, lk);
    rc == Rc::Ok && v.bool_test()
}

type Out = (Rc, RsValue);

const fn fail() -> Out {
    (Rc::Err, RsValue::Null)
}

const fn num(n: f64) -> Out {
    (Rc::Ok, RsValue::Number(n))
}

const fn null() -> Out {
    (Rc::Ok, RsValue::Null)
}

/// A NaN the operation produced from non-NaN inputs is the platform's default NaN; the
/// reference (aarch64) makes it positive, so make it positive everywhere.
fn fresh_nan(r: f64, inputs: &[f64]) -> f64 {
    if r.is_nan() && !inputs.iter().any(|x| x.is_nan()) { f64::NAN } else { r }
}

enum Want {
    Type(u8, &'static str),
    Str,
}

impl EvalState {
    pub fn new(mode: Mode) -> Self {
        EvalState { mode, err: None }
    }

    /// `QueryError_Set*`: the first error wins.
    pub fn set_err(&mut self, e: QErr) {
        if self.err.is_none() {
            self.err = Some(e);
        }
    }

    /// `ExprEval_Eval`: the status and the value (null unless `Rc::Ok`, except a missing
    /// property's null).
    pub fn run(&mut self, expr: &Expr, lk: &dyn Lookup) -> (Rc, RsValue) {
        self.node(Some(expr), "", lk)
    }

    /// A pipeline row (`rpevalCommon`): the value, or the pending error when the
    /// evaluation failed. A success may leave an error pending in `self.err`.
    pub fn eval_row(&mut self, expr: &Expr, lk: &dyn Lookup) -> Result<RsValue, QErr> {
        match self.run(expr, lk) {
            (Rc::Ok, v) => Ok(v),
            _ => Err(self.err.clone().unwrap_or_else(|| QErr::code(Code::Generic))),
        }
    }

    fn node(&mut self, e: Option<&Expr>, label: &str, lk: &dyn Lookup) -> Out {
        let Some(e) = e else {
            // The reference would dereference the NULL child; the lookup-key pass that runs
            // before every evaluation rejects such trees, so report its error.
            self.set_err(missing_child(label));
            return fail();
        };
        match e {
            Expr::Literal(l) => (
                Rc::Ok,
                match l {
                    Literal::Str(s) => RsValue::String(s.clone()),
                    Literal::Num(n) => RsValue::Number(*n),
                    Literal::Null => RsValue::Null,
                },
            ),
            Expr::Property(name) => match lk.get(name) {
                Some(v) => (Rc::Ok, v),
                None => {
                    if self.mode == Mode::Query {
                        self.set_err(QErr::user(
                            Code::NoPropVal,
                            "Could not find the value for a parameter name, consider using EXISTS if applicable",
                            format!(" for {}", String::from_utf8_lossy(name)),
                        ));
                    }
                    (Rc::Missing, RsValue::Null)
                }
            },
            Expr::Func { func, args } => self.func(*func, args, lk),
            Expr::Op { op, left, right } => self.op(*op, left.as_deref(), right.as_deref(), lk),
            Expr::Pred { cond, left, right } => self.pred(*cond, left.as_deref(), right.as_deref(), lk),
            Expr::Inverted(c) => match self.node(c.as_deref(), "expr->inverted.child", lk) {
                (Rc::Ok, v) => num(if v.bool_test() { 0.0 } else { 1.0 }),
                _ => fail(),
            },
        }
    }

    fn op(&mut self, op: u8, l: Option<&Expr>, r: Option<&Expr>, lk: &dyn Lookup) -> Out {
        let (Rc::Ok, a) = self.node(l, "expr->op.left", lk) else { return fail() };
        let (Rc::Ok, b) = self.node(r, "expr->op.right", lk) else { return fail() };
        let (Some(x), Some(y)) = (a.to_number(), b.to_number()) else {
            self.set_err(QErr::code(Code::NumericValueInvalid));
            return fail();
        };
        let res = match op {
            b'+' => x + y,
            b'/' => x / y,
            b'-' => x - y,
            b'*' => x * y,
            b'%' => x % y,
            _ => x.powf(y),
        };
        num(fresh_nan(res, &[x, y]))
    }

    fn pred(&mut self, cond: Cond, l: Option<&Expr>, r: Option<&Expr>, lk: &dyn Lookup) -> Out {
        let logical = matches!(cond, Cond::And | Cond::Or);
        let mut vals = [RsValue::Null, RsValue::Null];
        for (i, (side, label)) in [(l, "expr->pred.left"), (r, "expr->pred.right")].into_iter().enumerate() {
            let (rc, v) = self.node(side, label, lk);
            match rc {
                Rc::Err => return fail(),
                Rc::Missing if !logical => {
                    return if self.mode == Mode::Query { fail() } else { num(0.0) };
                }
                _ => {}
            }
            if logical {
                let b = v.bool_test();
                if (cond == Cond::And && !b) || (cond == Cond::Or && b) {
                    return num(if b { 1.0 } else { 0.0 });
                }
            }
            vals[i] = v;
        }
        let [a, b] = &vals;
        let res = match cond {
            Cond::Eq => a.equal(b),
            Cond::Ne => !a.equal(b),
            Cond::Lt => a.cmp_with(b, Some(&mut self.err)) == Ordering::Less,
            Cond::Le => a.cmp_with(b, Some(&mut self.err)) != Ordering::Greater,
            Cond::Gt => a.cmp_with(b, Some(&mut self.err)) == Ordering::Greater,
            Cond::Ge => a.cmp_with(b, Some(&mut self.err)) != Ordering::Less,
            Cond::And => a.bool_test() && b.bool_test(),
            Cond::Or => a.bool_test() || b.bool_test(),
        };
        if self.err.is_none() { num(if res { 1.0 } else { 0.0 }) } else { fail() }
    }

    /// `VALIDATE_ARG_TYPE` / `VALIDATE_ARG_ISSTRING`.
    fn check(&mut self, fname: &str, v: &RsValue, idx: usize, want: Want) -> bool {
        let (ok, verifier, varg) = match want {
            Want::Type(t, name) => (v.type_code() == t, "VALIDATE_ARG__TYPE", name),
            Want::Str => (v.is_string(), "VALIDATE_ARG__STRING", "0"),
        };
        if !ok {
            self.set_err(QErr::new(
                Code::ParseArgs,
                format!("Invalid type ({}) for argument {idx} in function '{fname}'. {verifier}(v, {varg}) was false.", v.type_code()),
            ));
        }
        ok
    }

    fn func(&mut self, func: usize, args: &[Option<Expr>], lk: &dyn Lookup) -> Out {
        let name = FUNCTIONS[func].name;
        let arg = |i: usize| args.get(i).and_then(Option::as_ref);
        if name == "case" {
            // Only the branch the condition picks is evaluated.
            let (rc, c) = self.node(arg(0), ARG, lk);
            if rc != Rc::Ok {
                return (rc, RsValue::Null);
            }
            return self.node(arg(if c.bool_test() { 1 } else { 2 }), ARG, lk);
        }
        let mut v = Vec::with_capacity(args.len());
        let mut missing = false;
        for i in 0..args.len() {
            let (rc, x) = self.node(arg(i), ARG, lk);
            match rc {
                Rc::Err => return fail(),
                Rc::Missing if name != "exists" => return fail(),
                Rc::Missing => missing = true,
                Rc::Ok => {}
            }
            v.push(x);
        }
        match name {
            "log" | "floor" | "abs" | "ceil" | "sqrt" | "log2" | "exp" => {
                let Some(d) = v[0].to_number() else { return num(f64::NAN) };
                let r = match name {
                    "log" => d.ln(),
                    "floor" => d.floor(),
                    "abs" => d.abs(),
                    "ceil" => d.ceil(),
                    "sqrt" => d.sqrt(),
                    "log2" => d.log2(),
                    _ => d.exp(),
                };
                num(fresh_nan(r, &[d]))
            }
            "timefmt" => self.timefmt(&v),
            "parsetime" => self.parsetime(&v),
            "hour" | "minute" | "day" | "month" | "monthofyear" | "year" | "dayofmonth" | "dayofweek" | "dayofyear" => date_func(name, &v[0]),
            "lower" | "upper" => match v[0].str_bytes() {
                None => null(),
                Some(s) => (
                    Rc::Ok,
                    RsValue::String(if name == "lower" { s.to_ascii_lowercase() } else { s.to_ascii_uppercase() }),
                ),
            },
            "substr" => self.substr(&v),
            "format" => self.format(&v),
            "split" => self.split(&v),
            "matched_terms" => {
                let mut max = 100;
                if let Some(d) = v.first().and_then(RsValue::to_number)
                    && (1.0..=100.0).contains(&d)
                {
                    max = d as usize;
                }
                let terms = lk.matched_terms(max);
                if terms.is_empty() {
                    null()
                } else {
                    (Rc::Ok, RsValue::Array(terms.into_iter().take(max).map(RsValue::String).collect()))
                }
            }
            "to_number" => match v[0].to_number() {
                Some(n) => num(n),
                None => {
                    let p = v[0].str_bytes().map_or_else(|| "(null)".to_string(), |s| String::from_utf8_lossy(cstr(s)).into_owned());
                    self.set_err(QErr::user(Code::ParseArgs, "to_number: cannot convert string", format!(" '{p}'")));
                    fail()
                }
            },
            "to_str" => (
                Rc::Ok,
                RsValue::String(match v[0].leaf() {
                    RsValue::String(s) | RsValue::RedisString(s) => s.clone(),
                    RsValue::Number(n) => num_to_str(*n).into_bytes(),
                    _ => Vec::new(),
                }),
            ),
            "exists" => {
                if missing {
                    self.err = None;
                    num(0.0)
                } else {
                    num(1.0)
                }
            }
            "startswith" | "contains" | "strlen" => self.str_test(name, &v),
            "geodistance" => self.geodistance(&v),
            _ => fail(),
        }
    }

    /// `timefmt(time[, format])`: `strftime` in UTC, `%FT%TZ` by default; null when the
    /// time is not a number, out of range, or the format yields nothing.
    fn timefmt(&mut self, v: &[RsValue]) -> Out {
        let mut fmt: &[u8] = b"%FT%TZ";
        if v.len() == 2 {
            if !self.check("time", &v[1], 1, Want::Type(2, "RSValueType_String")) {
                return fail();
            }
            fmt = cstr(v[1].str_bytes().unwrap_or_default());
        }
        let Some(n) = v[0].to_number() else { return null() };
        let Some(tm) = secs_to_tm(n as i64) else { return null() };
        match strftime(fmt, &tm, 1024) {
            Some(s) if !s.is_empty() => (Rc::Ok, RsValue::String(s)),
            _ => null(),
        }
    }

    /// `parsetime(text, format)`: `strptime` into a zeroed `tm`, then `timegm`.
    fn parsetime(&mut self, v: &[RsValue]) -> Out {
        if !self.check("parsetime", &v[0], 0, Want::Str) || !self.check("parsetime", &v[1], 1, Want::Str) {
            return fail();
        }
        let text = cstr(v[0].str_bytes().unwrap_or_default());
        let fmt = cstr(v[1].str_bytes().unwrap_or_default());
        let mut tm = Tm::default();
        if strptime(text, 0, fmt, &mut tm).is_none() {
            return null();
        }
        num(timegm(&tm) as f64)
    }

    /// `substr(str, offset, len)`: negative offsets count from the end, a negative length
    /// stops that many bytes before it. Offsets and lengths are C ints (saturating casts).
    fn substr(&mut self, v: &[RsValue]) -> Out {
        if !self.check("substr", &v[1], 1, Want::Type(1, "RSValueType_Number"))
            || !self.check("substr", &v[2], 2, Want::Type(1, "RSValueType_Number"))
        {
            return fail();
        }
        let Some(s) = v[0].str_bytes() else {
            self.set_err(QErr::new(Code::ParseArgs, "Invalid type for substr. Expected string"));
            return fail();
        };
        let sz = s.len();
        let as_int = |x: &RsValue| match x {
            RsValue::Number(n) => *n as i32,
            _ => 0,
        };
        let mut offset = as_int(&v[1]);
        let mut len = as_int(&v[2]);
        if offset < 0 {
            offset = (sz as i32).wrapping_add(offset);
        }
        // MIN(offset, sz) compares as size_t: a still-negative offset becomes sz.
        if offset < 0 || offset as usize > sz {
            offset = sz as i32;
        }
        if len < 0 {
            len = (sz as i32).wrapping_sub(offset).wrapping_add(len).max(0);
        }
        if offset.wrapping_add(len) as i64 as u64 > sz as u64 {
            len = (sz - offset as usize) as i32;
        }
        let o = offset as usize;
        (Rc::Ok, RsValue::String(s[o..o + len as usize].to_vec()))
    }

    /// `format(fmt, args...)`: only `%s` (strings, numbers, `(null)`; other values print
    /// nothing) and `%%`.
    fn format(&mut self, v: &[RsValue]) -> Out {
        if !self.check("format", &v[0], 0, Want::Str) {
            return fail();
        }
        let fmt = v[0].str_bytes().unwrap_or_default();
        let mut out = Vec::with_capacity(fmt.len());
        let mut argix = 1;
        let mut last = 0;
        let mut i = 0;
        while i < fmt.len() {
            if fmt[i] != b'%' {
                i += 1;
                continue;
            }
            if i == fmt.len() - 1 {
                self.set_err(QErr::new(Code::ParseArgs, "Bad format string!"));
                return fail();
            }
            out.extend_from_slice(&fmt[last..i]);
            last = i + 2;
            i += 1;
            let ty = fmt[i];
            i += 1;
            if ty == b'%' {
                out.push(b'%');
                continue;
            }
            if argix == v.len() {
                self.set_err(QErr::new(Code::ParseArgs, "Not enough arguments for format"));
                return fail();
            }
            let a = v[argix].leaf();
            argix += 1;
            if ty != b's' {
                self.set_err(QErr::new(Code::ParseArgs, "Unknown format specifier passed"));
                return fail();
            }
            match a {
                RsValue::Null => out.extend_from_slice(b"(null)"),
                RsValue::String(s) | RsValue::RedisString(s) => out.extend_from_slice(s),
                RsValue::Number(n) => out.extend_from_slice(num_to_str(*n).as_bytes()),
                _ => {}
            }
        }
        if last < fmt.len() {
            out.extend_from_slice(&fmt[last..]);
        }
        (Rc::Ok, RsValue::String(out))
    }

    /// `split(str[, separators[, strip]])`: separators default to `,`, the strip set to a
    /// space; empty pieces are dropped, at most 1024 kept.
    fn split(&mut self, v: &[RsValue]) -> Out {
        if !self.check("split", &v[0], 0, Want::Str) {
            return fail();
        }
        let mut sep: &[u8] = b",";
        let mut strip: &[u8] = b" ";
        if v.len() >= 2 {
            if !self.check("split", &v[1], 1, Want::Str) {
                return fail();
            }
            sep = cstr(v[1].str_bytes().unwrap_or_default());
        }
        if v.len() == 3 {
            if !self.check("split", &v[2], 2, Want::Str) {
                return fail();
            }
            strip = cstr(v[2].str_bytes().unwrap_or_default());
        }
        let s = v[0].str_bytes().unwrap_or_default();
        // `strchr(set, c)` finds the terminator for c == 0, so NULs always strip.
        let in_strip = |c: u8| c == 0 || strip.contains(&c);
        let mut out = Vec::new();
        let mut tok = 0;
        while out.len() < 1024 && tok < s.len() {
            // `strpbrk` stops at the first NUL.
            let next = s[tok..].iter().take_while(|&&c| c != 0).position(|c| sep.contains(c)).map(|p| tok + p);
            let piece = &s[tok..next.unwrap_or(s.len())];
            if !piece.is_empty() {
                let start = piece.iter().position(|&c| !in_strip(c)).unwrap_or(piece.len());
                let end = piece.iter().rposition(|&c| !in_strip(c)).map_or(start, |e| (e + 1).max(start));
                if end > start {
                    out.push(RsValue::String(piece[start..end].to_vec()));
                }
            }
            match next {
                None => break,
                Some(n) => tok = n + 1,
            }
        }
        (Rc::Ok, RsValue::Array(out))
    }

    /// `startswith`, `contains` (overlapping matches; an empty needle counts length + 1) and
    /// `strlen` (bytes).
    fn str_test(&mut self, name: &str, v: &[RsValue]) -> Out {
        for (i, x) in v.iter().enumerate() {
            if !self.check(name, x, i, Want::Str) {
                return fail();
            }
        }
        let s = v[0].str_bytes().unwrap_or_default();
        match name {
            "strlen" => num(s.len() as f64),
            "startswith" => {
                // `strncmp(prefix, str, strlen-of-prefix-value)`: stops at a shared NUL.
                let p = v[1].str_bytes().unwrap_or_default();
                let mut eq = true;
                for (i, &a) in p.iter().enumerate() {
                    if a != s.get(i).copied().unwrap_or(0) {
                        eq = false;
                        break;
                    }
                    if a == 0 {
                        break;
                    }
                }
                num(if eq { 1.0 } else { 0.0 })
            }
            _ => {
                let p = v[1].str_bytes().unwrap_or_default();
                let (hay, needle) = (cstr(s), cstr(p));
                let n = if p.is_empty() {
                    s.len() + 1
                } else if needle.is_empty() {
                    // A needle starting with NUL: `strstr` matches everywhere and the
                    // reference walks past the terminator (undefined behaviour); count the
                    // positions up to it.
                    hay.len() + 1
                } else {
                    hay.windows(needle.len()).filter(|w| *w == needle).count()
                };
                num(n as f64)
            }
        }
    }

    /// `geodistance`: two "lon,lat" strings or geohash numbers, two numbers and a string (or
    /// the reverse), or four numbers; meters rounded to centimetres. Unparsable input gives
    /// NaN, and a malformed string also sets an error.
    fn geodistance(&mut self, v: &[RsValue]) -> Out {
        let lonlat = |a: &RsValue, b: &RsValue| Some([a.to_number()?, b.to_number()?]);
        let mut points = [[0.0; 2]; 2];
        let ok = match v.len() {
            2 => self.geo_field(&v[0]).zip(self.geo_field(&v[1])).map(|(a, b)| points = [a, b]).is_some(),
            4 => lonlat(&v[0], &v[1]).zip(lonlat(&v[2], &v[3])).map(|(a, b)| points = [a, b]).is_some(),
            _ if v[0].to_number().is_some() => lonlat(&v[0], &v[1]).and_then(|a| Some((a, self.geo_field(&v[2])?))).map(|(a, b)| points = [a, b]).is_some(),
            _ => self.geo_field(&v[0]).and_then(|a| Some((a, lonlat(&v[1], &v[2])?))).map(|(a, b)| points = [a, b]).is_some(),
        };
        if !ok {
            return num(f64::NAN);
        }
        let [[lon1, lat1], [lon2, lat2]] = points;
        let d = haversine(lon1, lat1, lon2, lat2);
        num((d * 100.0).round() / 100.0)
    }

    /// `parseField`: a "lon,lat" string or a geohash number.
    fn geo_field(&mut self, v: &RsValue) -> Option<[f64; 2]> {
        match v {
            RsValue::String(s) | RsValue::RedisString(s) => match parse_geo(s) {
                Ok(p) => Some(p),
                Err(m) => {
                    self.set_err(QErr::new(Code::ParseArgs, m));
                    None
                }
            },
            RsValue::Number(n) => Some(decode_geo(*n)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Geo (`parseGeo`, `decodeGeo`, `geohashGetDistance`).

const GEO_LAT_MIN: f64 = -85.05112878;
const GEO_LAT_MAX: f64 = 85.05112878;
const GEO_LONG_MIN: f64 = -180.0;
const GEO_LONG_MAX: f64 = 180.0;
const EARTH_RADIUS_IN_METERS: f64 = 6372797.560856;

/// "lon,lat" or "lon lat", each side trimmed; the text of the error otherwise.
fn parse_geo(b: &[u8]) -> Result<[f64; 2], String> {
    let s = std::str::from_utf8(b).map_err(|_| "Invalid geo string: not valid UTF-8".to_string())?;
    let (lon, lat) = s.split_once([',', ' ']).ok_or_else(|| "Invalid geo string: missing separator".to_string())?;
    let coord = |t: &str| -> Result<f64, String> {
        let t = t.trim();
        let x: f64 = t.parse().map_err(|e: std::num::ParseFloatError| format!("Invalid geo string {t:?}: {e}"))?;
        if x.is_finite() { Ok(x) } else { Err("Geo coordinates must be finite".to_string()) }
    };
    Ok([coord(lon)?, coord(lat)?])
}

/// A 52-bit geohash score (as a double) to the centre of its cell.
fn decode_geo(bits: f64) -> [f64; 2] {
    let raw = bits as u64;
    let (mut ilat, mut ilon) = (0u32, 0u32);
    for i in 0..32 {
        ilat |= (((raw >> (2 * i)) & 1) as u32) << i;
        ilon |= (((raw >> (2 * i + 1)) & 1) as u32) << i;
    }
    let step = (1u64 << 26) as f64;
    let lon = (GEO_LONG_MIN + ((ilon as f64 + 0.5) / step) * (GEO_LONG_MAX - GEO_LONG_MIN)).clamp(GEO_LONG_MIN, GEO_LONG_MAX);
    let lat = (GEO_LAT_MIN + ((ilat as f64 + 0.5) / step) * (GEO_LAT_MAX - GEO_LAT_MIN)).clamp(GEO_LAT_MIN, GEO_LAT_MAX);
    [lon, lat]
}

/// Great-circle distance in meters (haversine); NaN for non-finite coordinates.
fn haversine(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    if ![lon1, lat1, lon2, lat2].iter().all(|x| x.is_finite()) {
        return f64::NAN;
    }
    let (phi1, lam1, phi2, lam2) = (lat1.to_radians(), lon1.to_radians(), lat2.to_radians(), lon2.to_radians());
    let u = ((phi2 - phi1) / 2.0).sin();
    let v = ((lam2 - lam1) / 2.0).sin();
    let h = u * u + phi1.cos() * phi2.cos() * v * v;
    2.0 * EARTH_RADIUS_IN_METERS * h.sqrt().min(1.0).asin()
}

// ---------------------------------------------------------------------------------------
// Dates: musl's `gmtime_r`, `timegm`, `strftime` and `strptime` (C locale, UTC).

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
}

/// `__secs_to_tm` (`gmtime_r`): `None` when the year would overflow an int.
fn secs_to_tm(t: i64) -> Option<Tm> {
    const LEAPOCH: i64 = 946684800 + 86400 * (31 + 29);
    const DAYS_PER_400Y: i32 = 365 * 400 + 97;
    const DAYS_PER_100Y: i32 = 365 * 100 + 24;
    const DAYS_PER_4Y: i32 = 365 * 4 + 1;
    const DAYS_IN_MONTH: [i32; 12] = [31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 31, 29];
    if t < i32::MIN as i64 * 31622400 || t > i32::MAX as i64 * 31622400 {
        return None;
    }
    let secs = t - LEAPOCH;
    let mut days = secs / 86400;
    let mut remsecs = (secs % 86400) as i32;
    if remsecs < 0 {
        remsecs += 86400;
        days -= 1;
    }
    let mut wday = ((3 + days) % 7) as i32;
    if wday < 0 {
        wday += 7;
    }
    let mut qc_cycles = (days / DAYS_PER_400Y as i64) as i32;
    let mut remdays = (days % DAYS_PER_400Y as i64) as i32;
    if remdays < 0 {
        remdays += DAYS_PER_400Y;
        qc_cycles -= 1;
    }
    let c_cycles = (remdays / DAYS_PER_100Y).min(3);
    remdays -= c_cycles * DAYS_PER_100Y;
    let q_cycles = (remdays / DAYS_PER_4Y).min(24);
    remdays -= q_cycles * DAYS_PER_4Y;
    let remyears = (remdays / 365).min(3);
    remdays -= remyears * 365;
    let leap = i32::from(remyears == 0 && (q_cycles != 0 || c_cycles == 0));
    let mut yday = remdays + 31 + 28 + leap;
    if yday >= 365 + leap {
        yday -= 365 + leap;
    }
    let mut years = remyears as i64 + 4 * q_cycles as i64 + 100 * c_cycles as i64 + 400 * qc_cycles as i64;
    let mut months = 0;
    while months < 12 && DAYS_IN_MONTH[months as usize] <= remdays {
        remdays -= DAYS_IN_MONTH[months as usize];
        months += 1;
    }
    if months >= 10 {
        months -= 12;
        years += 1;
    }
    if years + 100 > i32::MAX as i64 || years + 100 < i32::MIN as i64 {
        return None;
    }
    Some(Tm {
        year: (years + 100) as i32,
        mon: months + 2,
        mday: remdays + 1,
        wday,
        yday,
        hour: remsecs / 3600,
        min: remsecs / 60 % 60,
        sec: remsecs % 60,
    })
}

/// `__year_to_secs`: seconds from the epoch to 1 January of `year` (years since 1900).
fn year_to_secs(year: i64) -> (i64, bool) {
    if (year as u64).wrapping_sub(2) <= 136 {
        let y = year as i32;
        let mut leaps = (y - 68) >> 2;
        let leap = (y - 68) & 3 == 0;
        if leap {
            leaps -= 1;
        }
        return (31536000 * (y - 70) as i64 + 86400 * leaps as i64, leap);
    }
    let mut cycles = ((year - 100) / 400) as i32;
    let mut rem = ((year - 100) % 400) as i32;
    if rem < 0 {
        cycles -= 1;
        rem += 400;
    }
    let (centuries, mut leaps, leap);
    if rem == 0 {
        leap = true;
        centuries = 0;
        leaps = 0;
    } else {
        centuries = rem / 100;
        rem -= centuries * 100;
        if rem == 0 {
            leap = false;
            leaps = 0;
        } else {
            leaps = rem / 4;
            leap = rem % 4 == 0;
        }
    }
    leaps += 97 * cycles + 24 * centuries - i32::from(leap);
    ((year - 100) * 31536000 + leaps as i64 * 86400 + 946684800 + 86400, leap)
}

/// `__tm_to_secs`: the fields as a UTC time, months normalized.
fn tm_to_secs(tm: &Tm) -> i64 {
    const THROUGH: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut year = tm.year as i64;
    let mut month = tm.mon;
    if !(0..12).contains(&month) {
        let mut adj = month / 12;
        month %= 12;
        if month < 0 {
            adj -= 1;
            month += 12;
        }
        year += adj as i64;
    }
    let (mut t, leap) = year_to_secs(year);
    t += THROUGH[month as usize] * 86400;
    if leap && month >= 2 {
        t += 86400;
    }
    t + 86400 * tm.mday.wrapping_sub(1) as i64 + 3600 * tm.hour as i64 + 60 * tm.min as i64 + tm.sec as i64
}

/// `timegm`: -1 when the result is out of `gmtime`'s range.
fn timegm(tm: &Tm) -> i64 {
    let t = tm_to_secs(tm);
    if secs_to_tm(t).is_none() { -1 } else { t }
}

/// `fast_timegm` (`date.c`): `tm_yday` based, with its leap-year shortcut (right until 2100).
fn fast_timegm(tm: &Tm) -> i64 {
    let years = tm.year as i64 - 70;
    let leaps = (years + 1) / 4;
    let days = tm.yday as i64 + years * 365 + leaps;
    days * 86400 + tm.hour as i64 * 3600 + tm.min as i64 * 60 + tm.sec as i64
}

/// The `hour`..`dayofyear` functions: null for non-numbers and negative times. A time
/// `gmtime` rejects leaves the zeroed `tm` in place.
fn date_func(name: &str, v: &RsValue) -> Out {
    let Some(d) = v.to_number() else { return null() };
    if d < 0.0 {
        return null();
    }
    if name == "minute" {
        return num(fresh_nan((d - d % 60.0).floor(), &[d]));
    }
    let mut tm = secs_to_tm(d as i64).unwrap_or_default();
    let r = match name {
        "hour" => {
            tm.sec = 0;
            tm.min = 0;
            fast_timegm(&tm) as f64
        }
        "day" => {
            tm.sec = 0;
            tm.min = 0;
            tm.hour = 0;
            fast_timegm(&tm) as f64
        }
        "month" => {
            tm.sec = 0;
            tm.min = 0;
            tm.hour = 0;
            tm.yday -= tm.mday - 1;
            fast_timegm(&tm) as f64
        }
        "monthofyear" => tm.mon as f64,
        "year" => tm.year as f64 + 1900.0,
        "dayofmonth" => tm.mday as f64,
        "dayofweek" => tm.wday as f64,
        _ => tm.yday as f64,
    };
    num(r)
}

const ABDAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAY: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const ABMON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MON: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `strtoul` over the digits at `i`: the value (saturating) and the index after them.
fn strtoul(b: &[u8], mut i: usize) -> (u64, usize) {
    let mut v: u64 = 0;
    while at(b, i).is_ascii_digit() {
        v = v.saturating_mul(10).saturating_add((b[i] - b'0') as u64);
        i += 1;
    }
    (v, i)
}

fn is_leap(mut y: i32) -> bool {
    if y > i32::MAX - 1900 {
        y -= 2000;
    }
    let y = y + 1900;
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// ISO 8601 week number (musl's `week_num`, unsigned arithmetic included).
fn week_num(tm: &Tm) -> i32 {
    let (yday, wday) = (tm.yday as u32, tm.wday as u32);
    let mut val = (yday.wrapping_add(7).wrapping_sub(wday.wrapping_add(6) % 7) / 7) as i32;
    if wday.wrapping_add(371).wrapping_sub(yday).wrapping_sub(2) % 7 <= 2 {
        val += 1;
    }
    if val == 0 {
        val = 52;
        let dec31 = wday.wrapping_add(7).wrapping_sub(yday).wrapping_sub(1) % 7;
        if dec31 == 4 || (dec31 == 5 && is_leap(tm.year % 400 - 1)) {
            val += 1;
        }
    } else if val == 53 {
        let jan1 = wday.wrapping_add(371).wrapping_sub(yday) % 7;
        if jan1 != 4 && (jan1 != 3 || !is_leap(tm.year)) {
            val = 1;
        }
    }
    val
}

/// `__strftime_fmt_1`: one conversion, `None` for an unknown one.
fn strftime_one(f: u8, tm: &Tm, pad: u8) -> Option<Vec<u8>> {
    let mut width = 2usize;
    let mut def_pad = b'0';
    let name = |list: &[&str], i: i32| list.get(i as u32 as usize).map_or(b"-".to_vec(), |s| s.as_bytes().to_vec());
    let sub = |fmt: &[u8]| strftime(fmt, tm, 100).filter(|s| !s.is_empty());
    let val: i64 = match f {
        b'a' => return Some(name(&ABDAY, tm.wday)),
        b'A' => return Some(name(&DAY, tm.wday)),
        b'h' | b'b' => return Some(name(&ABMON, tm.mon)),
        b'B' => return Some(name(&MON, tm.mon)),
        b'c' => return sub(b"%a %b %e %T %Y"),
        b'C' => (1900 + tm.year as i64) / 100,
        b'e' => {
            def_pad = b'_';
            tm.mday as i64
        }
        b'd' => tm.mday as i64,
        b'D' | b'x' => return sub(b"%m/%d/%y"),
        b'F' => return sub(b"%Y-%m-%d"),
        b'g' | b'G' => {
            let mut v = tm.year as i64 + 1900;
            if tm.yday < 3 && week_num(tm) != 1 {
                v -= 1;
            } else if tm.yday > 360 && week_num(tm) == 1 {
                v += 1;
            }
            if f == b'g' {
                v %= 100;
            } else {
                width = 4;
            }
            v
        }
        b'H' => tm.hour as i64,
        b'I' => match tm.hour {
            0 => 12,
            h if h > 12 => h as i64 - 12,
            h => h as i64,
        },
        b'j' => {
            width = 3;
            tm.yday as i64 + 1
        }
        b'm' => tm.mon as i64 + 1,
        b'M' => tm.min as i64,
        b'n' => return Some(b"\n".to_vec()),
        b'p' => return Some(if tm.hour >= 12 { b"PM".to_vec() } else { b"AM".to_vec() }),
        b'r' => return sub(b"%I:%M:%S %p"),
        b'R' => return sub(b"%H:%M"),
        b's' => {
            width = 1;
            tm_to_secs(tm)
        }
        b'S' => tm.sec as i64,
        b't' => return Some(b"\t".to_vec()),
        b'T' | b'X' => return sub(b"%H:%M:%S"),
        b'u' => {
            width = 1;
            if tm.wday != 0 { tm.wday as i64 } else { 7 }
        }
        b'U' => ((tm.yday as u32).wrapping_add(7).wrapping_sub(tm.wday as u32) / 7) as i64,
        b'W' => ((tm.yday as u32).wrapping_add(7).wrapping_sub((tm.wday as u32).wrapping_add(6) % 7) / 7) as i64,
        b'V' => week_num(tm) as i64,
        b'w' => {
            width = 1;
            tm.wday as i64
        }
        b'y' => ((tm.year as i64 + 1900) % 100).abs(),
        b'Y' => {
            let v = tm.year as i64 + 1900;
            if v >= 10000 {
                return Some(format!("+{v}").into_bytes());
            }
            width = 4;
            v
        }
        // gmtime's tm: no DST, zero offset, zone "UTC".
        b'z' => return Some(b"+0000".to_vec()),
        b'Z' => return Some(b"UTC".to_vec()),
        b'%' => return Some(b"%".to_vec()),
        _ => return None,
    };
    Some(
        match if pad != 0 { pad } else { def_pad } {
            b'-' => format!("{val}"),
            b'_' => format!("{val:>width$}"),
            _ => format!("{val:0width$}"),
        }
        .into_bytes(),
    )
}

/// `__strftime_l` with an `n`-byte buffer: the output, or `None` where it returns 0 (an
/// unknown conversion, or output that does not fit with its terminator).
fn strftime(f: &[u8], tm: &Tm, n: usize) -> Option<Vec<u8>> {
    let mut s: Vec<u8> = Vec::new();
    let mut i = 0;
    while s.len() < n {
        let c = at(f, i);
        if c == 0 {
            return Some(s);
        }
        if c != b'%' {
            s.push(c);
            i += 1;
            continue;
        }
        i += 1;
        let mut pad = 0;
        if matches!(at(f, i), b'-' | b'_' | b'0') {
            pad = at(f, i);
            i += 1;
        }
        let plus = at(f, i) == b'+';
        if plus {
            i += 1;
        }
        let (mut width, p) = if at(f, i).is_ascii_digit() { strtoul(f, i) } else { (0, i) };
        if matches!(at(f, p), b'C' | b'F' | b'G' | b'Y') {
            if width == 0 && p != i {
                width = 1;
            }
        } else {
            width = 0;
        }
        i = p;
        if matches!(at(f, i), b'E' | b'O') {
            i += 1;
        }
        let out = strftime_one(at(f, i), tm, pad)?;
        let mut t: &[u8] = &out;
        if width > 0 {
            if matches!(t.first(), Some(b'+' | b'-')) {
                t = &t[1..];
            }
            while t.first() == Some(&b'0') && t.get(1).is_some_and(u8::is_ascii_digit) {
                t = &t[1..];
            }
            let k = t.len() as u64;
            width = width.max(k);
            let d = t.iter().take_while(|c| c.is_ascii_digit()).count() as u64;
            if tm.year < -1900 {
                s.push(b'-');
                width -= 1;
            } else if plus && d + (width - k) >= if at(f, p) == b'C' { 3 } else { 5 } {
                s.push(b'+');
                width -= 1;
            }
            while width > k && s.len() < n {
                s.push(b'0');
                width -= 1;
            }
        }
        let room = n - s.len();
        s.extend_from_slice(&t[..t.len().min(room)]);
        i += 1;
    }
    None
}

/// `strncasecmp(s + i, word, strlen(word)) == 0`.
fn prefix_ci(s: &[u8], i: usize, word: &[u8]) -> bool {
    s.get(i..i + word.len()).is_some_and(|x| x.eq_ignore_ascii_case(word))
}

/// `numeric_range`: at most as many digits as `min + range` has, the value in range.
fn strp_range(s: &[u8], si: &mut usize, min: i32, range: i32, adj: i32) -> Option<i32> {
    if !at(s, *si).is_ascii_digit() {
        return None;
    }
    let mut v: i32 = 0;
    let mut i = 1;
    while i <= min + range && at(s, *si).is_ascii_digit() {
        v = v.wrapping_mul(10).wrapping_add((s[*si] - b'0') as i32);
        *si += 1;
        i *= 10;
    }
    if v.wrapping_sub(min) as u32 >= range as u32 {
        return None;
    }
    Some(v - adj)
}

/// `numeric_digits`: an optional sign and up to `w` digits.
fn strp_digits(s: &[u8], si: &mut usize, w: i32, adj: i32) -> Option<i32> {
    let mut neg = false;
    if at(s, *si) == b'+' {
        *si += 1;
    } else if at(s, *si) == b'-' {
        neg = true;
        *si += 1;
    }
    if !at(s, *si).is_ascii_digit() {
        return None;
    }
    let mut v: i32 = 0;
    let mut i = 0;
    while i < w && at(s, *si).is_ascii_digit() {
        v = v.wrapping_mul(10).wrapping_add((s[*si] - b'0') as i32);
        *si += 1;
        i += 1;
    }
    if neg {
        v = v.wrapping_neg();
    }
    Some(v.wrapping_sub(adj))
}

/// `symbolic_range`: full names first, then abbreviations, case-insensitively.
fn strp_name(s: &[u8], si: &mut usize, abbr: &[&str], full: &[&str]) -> Option<i32> {
    let range = abbr.len();
    for i in (0..2 * range).rev() {
        let ex = if i < range { abbr[i] } else { full[i - range] }.as_bytes();
        if prefix_ci(s, *si, ex) {
            *si += ex.len();
            return Some((i % range) as i32);
        }
    }
    None
}

/// musl's `strptime` as the reference links it (no `%F`, `%s`, `%u`, `%V`, `%g`, `%G`,
/// `%z`, `%Z`): the position after the parsed text, `None` on a mismatch.
fn strptime(s: &[u8], mut si: usize, f: &[u8], tm: &mut Tm) -> Option<usize> {
    let mut fi = 0;
    let (mut want_century, mut century, mut relyear) = (0u8, 0i32, 0i32);
    while at(f, fi) != 0 {
        let fc = f[fi];
        if fc != b'%' {
            if is_space(fc) {
                while at(s, si) != 0 && is_space(s[si]) {
                    si += 1;
                }
            } else if at(s, si) != fc {
                return None;
            } else {
                si += 1;
            }
            fi += 1;
            continue;
        }
        fi += 1;
        if at(f, fi) == b'+' {
            fi += 1;
        }
        let mut w: i32 = -1;
        if at(f, fi).is_ascii_digit() {
            let (v, e) = strtoul(f, fi);
            w = v as u32 as i32;
            fi = e;
        }
        let conv = at(f, fi);
        fi += 1;
        match conv {
            b'a' | b'A' => tm.wday = strp_name(s, &mut si, &ABDAY, &DAY)?,
            b'b' | b'B' | b'h' => tm.mon = strp_name(s, &mut si, &ABMON, &MON)?,
            b'c' => si = strptime(s, si, b"%a %b %e %T %Y", tm)?,
            b'C' => {
                if w < 0 {
                    w = 2;
                }
                want_century |= 2;
                century = strp_digits(s, &mut si, w, 0)?;
            }
            b'd' | b'e' => tm.mday = strp_range(s, &mut si, 1, 31, 0)?,
            b'D' | b'x' => si = strptime(s, si, b"%m/%d/%y", tm)?,
            b'H' => tm.hour = strp_range(s, &mut si, 0, 24, 0)?,
            b'I' => tm.hour = strp_range(s, &mut si, 1, 12, 0)?,
            b'j' => tm.yday = strp_range(s, &mut si, 1, 366, 1)?,
            b'm' => tm.mon = strp_range(s, &mut si, 1, 12, 1)?,
            b'M' => tm.min = strp_range(s, &mut si, 0, 60, 0)?,
            b'n' | b't' => {
                while at(s, si) != 0 && is_space(s[si]) {
                    si += 1;
                }
            }
            b'p' => {
                if prefix_ci(s, si, b"AM") {
                    tm.hour %= 12;
                } else if prefix_ci(s, si, b"PM") {
                    tm.hour = tm.hour % 12 + 12;
                } else {
                    return None;
                }
                si += 2;
            }
            b'r' => si = strptime(s, si, b"%I:%M:%S %p", tm)?,
            b'R' => si = strptime(s, si, b"%H:%M", tm)?,
            b'S' => tm.sec = strp_range(s, &mut si, 0, 61, 0)?,
            b'T' | b'X' => si = strptime(s, si, b"%H:%M:%S", tm)?,
            b'U' | b'W' => {
                strp_range(s, &mut si, 0, 54, 0)?;
            }
            b'w' => tm.wday = strp_range(s, &mut si, 0, 7, 0)?,
            b'y' => {
                want_century |= 1;
                relyear = strp_digits(s, &mut si, 2, 0)?;
            }
            b'Y' => {
                if w < 0 {
                    w = 4;
                }
                want_century = 0;
                tm.year = strp_digits(s, &mut si, w, 1900)?;
            }
            b'%' => {
                if at(s, si) != b'%' {
                    return None;
                }
                si += 1;
            }
            _ => return None,
        }
    }
    if want_century != 0 {
        tm.year = relyear;
        if want_century & 2 != 0 {
            tm.year = tm.year.wrapping_add(century.wrapping_mul(100)).wrapping_sub(1900);
        } else if tm.year <= 68 {
            tm.year += 100;
        }
    }
    Some(si)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::expr::parse;

    struct Row(Vec<(&'static str, RsValue)>, Vec<&'static str>);

    impl Lookup for Row {
        fn get(&self, name: &[u8]) -> Option<RsValue> {
            self.0.iter().find(|(k, _)| k.as_bytes() == name).map(|(_, v)| v.clone())
        }
        fn matched_terms(&self, max: usize) -> Vec<Vec<u8>> {
            self.1.iter().take(max).map(|t| t.as_bytes().to_vec()).collect()
        }
    }

    fn rs(s: &str) -> RsValue {
        RsValue::RedisString(s.as_bytes().to_vec())
    }

    /// zzev:1 of the reference session: hash fields as Redis strings, `n` sortable numeric.
    fn doc1() -> Row {
        Row(
            vec![
                ("n", RsValue::Number(10.0)),
                ("s", rs("hello")),
                ("num", rs("3.5")),
                ("neg", rs("-2")),
                ("fmt", rs("%Y")),
                ("e", rs("")),
                ("geo", rs("2.35,48.85")),
                ("t", rs("Hello World")),
            ],
            vec![],
        )
    }

    fn doc2() -> Row {
        Row(vec![("n", RsValue::Number(20.0)), ("s", rs("abc"))], vec![])
    }

    /// The value as `redis-cli` printed it (RESP2), or the error text.
    fn show(v: &RsValue) -> String {
        match v {
            RsValue::Null => "(nil)".into(),
            RsValue::Number(n) => num_to_str(*n),
            RsValue::String(s) | RsValue::RedisString(s) => String::from_utf8_lossy(s).into_owned(),
            RsValue::Array(a) => format!("[{}]", a.iter().map(show).collect::<Vec<_>>().join(",")),
            other => format!("{other:?}"),
        }
    }

    fn run(src: &str, row: &Row, mode: Mode) -> String {
        let e = parse(src.as_bytes()).unwrap_or_else(|m| panic!("{src}: {m:?}"));
        match eval(&e, row, mode) {
            Ok(v) => show(&v),
            Err(e) => format!("ERR {}", e.text()),
        }
    }

    fn check(row: &Row, cases: &[(&str, &str)]) {
        let mut bad = Vec::new();
        for (src, want) in cases {
            let got = run(src, row, Mode::Query);
            if got != *want {
                bad.push(format!("{src}\n  want {want}\n  got  {got}"));
            }
        }
        assert!(bad.is_empty(), "{} mismatches:\n{}", bad.len(), bad.join("\n"));
    }

    #[test]
    fn arithmetic_and_formatting() {
        check(
            &doc1(),
            &[
                ("1/0", "inf"),
                ("-1/0", "-inf"),
                ("0/0", "nan"),
                ("5 % 3", "2"),
                ("5.5 % 2", "1.5"),
                ("-5.5 % 2", "-1.5"),
                ("2 ^ 10", "1024"),
                ("2 ^ 0.5", "1.41421356237"),
                ("1e20", "1e+20"),
                ("0.1 + 0.2", "0.3"),
                ("1/3", "0.333333333333"),
                ("123456789012", "123456789012"),
                ("1234567890123.5", "1.23456789012e+12"),
                ("-0.0 * 1", "0"),
                ("@num", "3.5"),
                ("@num * 2", "7"),
                ("@neg ^ 0.5", "nan"),
                ("1e300*1e300", "inf"),
                ("-1e300*1e300", "-inf"),
                ("sqrt(-1)", "nan"),
                ("log(0)", "-inf"),
                ("0.000012345", "1.2345e-05"),
                ("1e-5", "1e-05"),
                ("123456789012345678901234", "1.23456789012e+23"),
                ("9223372036854775807", "9.22337203685e+18"),
                ("-9223372036854775808", "-9223372036854775808"),
                ("9.2233720368547e18", "9223372036854700032"),
                ("@s + 1", "ERR SEARCH_NUMERIC_VALUE_INVALID Invalid numeric value"),
                ("NULL + 1", "ERR SEARCH_NUMERIC_VALUE_INVALID Invalid numeric value"),
                ("floor(2.5) + ceil(2.5) + abs(-1) + log2(8) + exp(0)", "10"),
                ("floor(\"x\")", "nan"),
            ],
        );
    }

    #[test]
    fn comparisons() {
        check(
            &doc1(),
            &[
                ("@s == \"hello\"", "1"),
                ("\"abc\" < \"abd\"", "1"),
                ("1 < \"2\"", "1"),
                ("\"1e1\" == 10", "1"),
                ("@num == \"3.5\"", "1"),
                ("!\"\"", "1"),
                ("!\"a\"", "0"),
                ("\"b\" > \"abc\"", "1"),
                ("@e || 0", "0"),
                ("NULL == 0", "0"),
                ("\"\" == NULL", "0"),
                ("\"inf\" > 1e308", "1"),
                ("\"nan\" == 1", "1"),
                ("\"x\" == \"x\" && 2", "1"),
                ("0/0 == 0/0", "1"),
                ("0/0 < 1", "0"),
                ("split(\"a,b\") == split(\"a,c\")", "1"),
                ("split(\"a\") < 1", "ERR Error converting string"),
                ("\"a\" < 1", "ERR Error converting string"),
                ("@s < 1", "ERR Error converting string"),
                ("@s == 1", "0"),
                ("@s != 1", "1"),
                ("\"10\" < 9", "0"),
                ("NULL < 1", "1"),
                ("NULL == NULL", "1"),
                ("@n >= 10 && @n <= 10", "1"),
            ],
        );
    }

    #[test]
    fn missing_properties() {
        let nf = "ERR SEARCH_VALUE_NOT_FOUND Could not find the value for a parameter name, consider using EXISTS if applicable for num";
        check(
            &doc2(),
            &[
                ("@num + 1", nf),
                ("exists(@num)", "0"),
                ("@num == 1", nf),
                ("@num && 1", nf),
                ("@num || 1", nf),
                ("1 || @num", "1"),
                ("!@num", nf),
                ("upper(@num)", nf),
                ("case(1, @num, 2)", nf),
                ("case(0, @num, 2)", "2"),
                ("exists(case(1, @num, 2))", "0"),
                // `exists` clears the error an earlier short circuit left pending.
                ("(@num && 1) + exists(@num)", "0"),
                ("(@num || 1) + exists(@num)", "1"),
                ("geodistance(\"abc\", \"1,2\") + exists(@num)", "nan"),
                ("geodistance(\"abc\", \"1,2\")", "ERR SEARCH_PARSE_ARGS Invalid geo string: missing separator"),
            ],
        );
    }

    #[test]
    fn pending_error_across_rows() {
        let e = parse(b"@num && 1").unwrap();
        let mut st = EvalState::new(Mode::Query);
        assert_eq!(st.eval_row(&e, &doc1()), Ok(RsValue::Number(1.0)));
        assert!(st.err.is_none());
        assert_eq!(st.eval_row(&e, &doc2()), Ok(RsValue::Number(0.0)));
        assert_eq!(st.err.as_ref().map(|e| e.code), Some(Code::NoPropVal));
        // The pending error now fails any comparison.
        let cmp = parse(b"1 == 1").unwrap();
        assert_eq!(st.eval_row(&cmp, &doc1()).map_err(|e| e.code), Err(Code::NoPropVal));
    }

    #[test]
    fn string_functions() {
        check(
            &doc1(),
            &[
                ("exists(NULL)", "1"),
                ("exists(1/0)", "1"),
                ("format(\"%s-%s\", NULL, 1.5)", "(null)-1.5"),
                ("format(\"%s\", @s)", "hello"),
                ("format(\"%d\", 1)", "ERR SEARCH_PARSE_ARGS Unknown format specifier passed"),
                ("format(\"abc%\")", "ERR SEARCH_PARSE_ARGS Bad format string!"),
                ("format(\"%s\")", "ERR SEARCH_PARSE_ARGS Not enough arguments for format"),
                ("format(\"%s %s\", 1)", "ERR SEARCH_PARSE_ARGS Not enough arguments for format"),
                ("format(\"100%%\")", "100%"),
                ("format(\"%s%s\", split(\"a,b\"), 1)", "1"),
                ("format(1)", "ERR SEARCH_PARSE_ARGS Invalid type (1) for argument 0 in function 'format'. VALIDATE_ARG__STRING(v, 0) was false."),
                ("to_str(NULL)", ""),
                ("to_str(1.50)", "1.5"),
                ("to_str(split(\"a\"))", ""),
                ("to_str(@s)", "hello"),
                ("to_number(\"0x10\")", "ERR SEARCH_PARSE_ARGS to_number: cannot convert string '0x10'"),
                ("to_number(\" 5\")", "ERR SEARCH_PARSE_ARGS to_number: cannot convert string ' 5'"),
                ("to_number(\"inf\")", "inf"),
                ("to_number(\"abc\")", "ERR SEARCH_PARSE_ARGS to_number: cannot convert string 'abc'"),
                ("to_number(split(\"a\"))", "ERR SEARCH_PARSE_ARGS to_number: cannot convert string '(null)'"),
                ("to_number(NULL)", "ERR SEARCH_PARSE_ARGS to_number: cannot convert string '(null)'"),
                ("to_number(\"1_000\")", "ERR SEARCH_PARSE_ARGS to_number: cannot convert string '1_000'"),
                ("to_number(\"+5\")", "5"),
                ("to_number(\".5\")", "0.5"),
                ("to_number(\"5.\")", "5"),
                ("to_number(\"Infinity\")", "inf"),
                ("to_number(\"-NaN\")", "-nan"),
                ("upper(1)", "(nil)"),
                ("lower(@t)", "hello world"),
                ("upper(NULL)", "(nil)"),
                ("lower(split(\"A,B\"))", "(nil)"),
                ("strlen(@s)", "5"),
                ("strlen(1)", "ERR SEARCH_PARSE_ARGS Invalid type (1) for argument 0 in function 'strlen'. VALIDATE_ARG__STRING(v, 0) was false."),
                ("strlen(NULL)", "ERR SEARCH_PARSE_ARGS Invalid type (3) for argument 0 in function 'strlen'. VALIDATE_ARG__STRING(v, 0) was false."),
                ("substr(\"hello\", -10, 2)", ""),
                ("substr(\"hello\", -2, 5)", "lo"),
                ("substr(\"hello\", 1, -1)", "ell"),
                ("substr(\"hello\", 1, -10)", ""),
                ("substr(\"hello\", 10, 2)", ""),
                ("substr(\"hello\", 1, 1e10)", "ello"),
                ("substr(\"hello\", 1e10, 1)", ""),
                ("substr(\"hello\", \"1\", 1)", "ERR SEARCH_PARSE_ARGS Invalid type (2) for argument 1 in function 'substr'. VALIDATE_ARG__TYPE(v, RSValueType_Number) was false."),
                ("substr(1, 1, 1)", "ERR SEARCH_PARSE_ARGS Invalid type for substr. Expected string"),
                ("substr(@s, 0.9, 2.9)", "he"),
                ("contains(\"aaaa\", \"aa\")", "3"),
                ("contains(\"abc\", \"\")", "4"),
                ("contains(@t, \"o\")", "2"),
                ("startswith(@s, \"he\")", "1"),
                ("startswith(@s, \"\")", "1"),
                ("startswith(\"he\", \"hello\")", "0"),
                ("split(\" a , b ,, c \")", "[a,b,c]"),
                ("split(\"a|b;c\", \"|;\")", "[a,b,c]"),
                ("split(\"xxaxx,b\", \",\", \"x\")", "[a,b]"),
                ("split(\"abc\", \"\")", "[abc]"),
                ("split(@s)", "[hello]"),
                ("split(1)", "ERR SEARCH_PARSE_ARGS Invalid type (1) for argument 0 in function 'split'. VALIDATE_ARG__STRING(v, 0) was false."),
                ("case(@s, \"y\", \"n\")", "y"),
                ("case(NULL, 1, 2)", "2"),
                ("case(split(\"\"), 1, 2)", "2"),
                ("matched_terms()", "(nil)"),
            ],
        );
    }

    #[test]
    fn upper_is_ascii_only() {
        let e = parse("upper(\"a\u{e9}\")".as_bytes()).unwrap();
        assert_eq!(eval(&e, &doc1(), Mode::Query), Ok(RsValue::String(b"A\xc3\xa9".to_vec())));
    }

    #[test]
    fn matched_terms() {
        let row = Row(vec![], vec!["hello", "world"]);
        assert_eq!(run("matched_terms()", &row, Mode::Query), "[hello,world]");
        assert_eq!(run("matched_terms(1)", &row, Mode::Query), "[hello]");
        assert_eq!(run("matched_terms(1000)", &row, Mode::Query), "[hello,world]");
    }

    #[test]
    fn date_functions() {
        check(
            &doc1(),
            &[
                ("timefmt(0)", "1970-01-01T00:00:00Z"),
                ("timefmt(1700000000.9)", "2023-11-14T22:13:20Z"),
                ("timefmt(-1)", "1969-12-31T23:59:59Z"),
                ("timefmt(1e20)", "(nil)"),
                ("timefmt(-1e20)", "(nil)"),
                ("timefmt(\"abc\")", "(nil)"),
                ("timefmt(0/0)", "1970-01-01T00:00:00Z"),
                ("timefmt(1, \"\")", "(nil)"),
                ("timefmt(1, @fmt)", "ERR SEARCH_PARSE_ARGS Invalid type (4) for argument 1 in function 'time'. VALIDATE_ARG__TYPE(v, RSValueType_String) was false."),
                ("timefmt(1, 5)", "ERR SEARCH_PARSE_ARGS Invalid type (1) for argument 1 in function 'time'. VALIDATE_ARG__TYPE(v, RSValueType_String) was false."),
                ("timefmt(-62167219200)", "0000-01-01T00:00:00Z"),
                ("timefmt(253402300800)", "+10000-01-01T00:00:00Z"),
                ("timefmt(1700000000, \"%10Y|%-d|%_d|%010s|%+4Y|%03e\")", "0000002023|14|14|1700000000|2023|14"),
                (
                    "timefmt(1700000000, \"%m|%M|%n|%p|%r|%R|%s|%S|%t|%T|%u|%U|%V|%w|%W|%x|%X|%y|%Y|%z|%Z|%%\")",
                    "11|13|\n|PM|10:13:20 PM|22:13|1700000000|20|\t|22:13:20|2|46|46|2|46|11/14/23|22:13:20|23|2023|+0000|UTC|%",
                ),
                ("timefmt(1700000000, \"%a %A %b %B %c|%C|%d|%D|%e|%F|%h|%H|%I|%j\")", "Tue Tuesday Nov November Tue Nov 14 22:13:20 2023|20|14|11/14/23|14|2023-11-14|Nov|22|10|318"),
                ("timefmt(1700000000, \"%k\")", "(nil)"),
                ("timefmt(1700000000, \"%\")", "(nil)"),
                ("timefmt(1704067200, \"%G-%V %g %U %W %j %u\")", "2024-01 24 00 01 001 1"),
                ("timefmt(1735603200, \"%G-%V %g %U %W %j\")", "2025-01 25 52 53 366"),
                ("timefmt(1609459200, \"%G-%V %g\")", "2020-53 20"),
                ("timefmt(-62135596800, \"%Y %C %y %G %+5Y %_5Y %-Y %8C %EY %Oy %+C\")", "0001 00 01 0001 +0001 0   1 1 00000000 0001 01 00"),
                ("timefmt(-62198668800, \"%Y|%C|%y|%G|%g|%6Y|%+6Y|%F|%3F\")", "-001|00|01|-002|-2|-00001|-00001|-001-01-02|-1-01-02"),
                ("timefmt(253402300800, \"%Y|%C|%F|%+12Y|%12Y\")", "+10000|100|+10000-01-01|+00000010000|000000010000"),
                ("timefmt(-62135596800, \"%_5Y|%e|%_3d|%-5Y\")", "0   1| 1| 1|00001"),
                ("timefmt(5, \"%_5Y\")", "01970"),
                ("timefmt(43200, \"%I %p %r\")", "12 PM 12:00:00 PM"),
                ("timefmt(0, \"%I %p\")", "12 AM"),
                ("hour(1700000000)", "1699999200"),
                ("hour(-1)", "(nil)"),
                ("hour(1e20)", "-2208988800"),
                ("hour(0/0)", "0"),
                ("hour(\"1700000000\")", "1699999200"),
                ("hour(\"x\")", "(nil)"),
                ("minute(1700000123.5)", "1700000100"),
                ("minute(0/0)", "nan"),
                ("day(1700000000)", "1699920000"),
                ("month(1700000000)", "1698796800"),
                ("monthofyear(1700000000)", "10"),
                ("year(1700000000)", "2023"),
                ("dayofmonth(1e20)", "0"),
                ("dayofweek(1700000000)", "2"),
                ("dayofyear(1700000000)", "317"),
                ("year(1e20)", "1900"),
                ("day(4102444800)", "4102444800"),
                ("month(4102444800)", "4102444800"),
                ("parsetime(\"2023-11-14\", \"%F\")", "(nil)"),
                ("parsetime(\"123\", \"%s\")", "(nil)"),
                ("parsetime(\"2023 3\", \"%Y %u\")", "(nil)"),
                ("parsetime(\"2023 +0100\", \"%Y %z\")", "(nil)"),
                ("parsetime(\"2023\", \"%Y\")", "1672444800"),
                ("parsetime(\"2023-02\", \"%Y-%m\")", "1675123200"),
                ("parsetime(\"Tue Nov 14 22:13:20 2023\", \"%c\")", "1700000000"),
                ("parsetime(\"2023-11-14 10:13:20 PM\", \"%Y-%m-%d %I:%M:%S %p\")", "1700000000"),
                ("parsetime(\"70\", \"%y\")", "-86400"),
                ("parsetime(\"69\", \"%y\")", "-31622400"),
                ("parsetime(\"x\", \"%Y\")", "(nil)"),
                ("parsetime(\"2023-11-14T22:13:20Zjunk\", \"%Y-%m-%dT%H:%M:%SZ\")", "1700000000"),
                ("parsetime(1, \"%Y\")", "ERR SEARCH_PARSE_ARGS Invalid type (1) for argument 0 in function 'parsetime'. VALIDATE_ARG__STRING(v, 0) was false."),
                ("parsetime(\"2023\", @fmt)", "1672444800"),
                ("parsetime(\"2023 100\", \"%Y %j\")", "1672444800"),
                ("parsetime(\"  2023\", \" %Y\")", "1672444800"),
                ("parsetime(\"20 23\", \"%C %y\")", "1672444800"),
                ("parsetime(\"2023 UTC\", \"%Y %Z\")", "(nil)"),
                ("parsetime(\"november 14 2023\", \"%B %d %Y\")", "1699920000"),
                ("parsetime(\"nov 14 2023\", \"%b %d %Y\")", "1699920000"),
            ],
        );
    }

    #[test]
    fn geodistance() {
        check(
            &doc1(),
            &[
                ("geodistance(1, 2, 3, 4)", "314371.93"),
                ("geodistance(\"1,2\", 3, 4)", "314371.93"),
                ("geodistance(1, 2, \"3,4\")", "314371.93"),
                ("geodistance(@geo, \"2.5 48.1\")", "84149.74"),
                ("geodistance(3471579339700058, \"1,2\")", "4364812.56"),
                ("geodistance(\"1,2\", NULL)", "nan"),
                ("geodistance(\"x\", 2, 3)", "ERR SEARCH_PARSE_ARGS Invalid geo string: missing separator"),
                ("geodistance(\"1,inf\", \"1,2\")", "ERR SEARCH_PARSE_ARGS Geo coordinates must be finite"),
                ("geodistance(\" 1 , 2\", \"1,2\")", "ERR SEARCH_PARSE_ARGS Invalid geo string \"\": cannot parse float from empty string"),
                ("geodistance(\"1,2,3\", \"1,2\")", "ERR SEARCH_PARSE_ARGS Invalid geo string \"2,3\": invalid float literal"),
                ("geodistance(\"\", \"1,2\")", "ERR SEARCH_PARSE_ARGS Invalid geo string: missing separator"),
                ("geodistance(\"1,\", \"1,2\")", "ERR SEARCH_PARSE_ARGS Invalid geo string \"\": cannot parse float from empty string"),
                ("geodistance(\"1, x\", \"3,4\")", "ERR SEARCH_PARSE_ARGS Invalid geo string \"x\": invalid float literal"),
            ],
        );
    }

    #[test]
    fn index_filters() {
        let row = Row(vec![("n", rs("20")), ("s", rs("hello"))], vec![]);
        let pass = |src: &str| filter_passes(&parse(src.as_bytes()).unwrap(), &row);
        assert!(!pass("@x == 1"));
        assert!(pass("!(@x == 1)"));
        assert!(pass("@x || 1"));
        assert!(pass("!exists(@x)"));
        assert!(!pass("@s < 1 || 1"));
        assert!(!pass("@x + 1 || 1"));
        assert!(pass("@n > 15"));
        assert!(!pass("@n > 25"));
        assert!(!pass("@x"));
        assert!(!pass("upper(@x) == \"A\""));
        // Index mode reports no missing-property error.
        assert_eq!(run("@x == 1", &row, Mode::Index), "0");
        assert_eq!(run("@x + 1", &row, Mode::Index), "ERR SEARCH_GENERIC Generic error evaluating the query");
    }

    #[test]
    fn missing_children() {
        let e = Expr::Op { op: b'+', left: Some(Box::new(Expr::Literal(Literal::Num(1.0)))), right: None };
        let err = eval(&e, &doc1(), Mode::Query).unwrap_err();
        assert_eq!(err.text(), "SEARCH_EXPR Missing (or badly formatted) value for expr->op.right");
        assert!(!filter_passes(&e, &doc1()));
        let f = Expr::Func { func: crate::search::expr::find_function(b"upper").unwrap(), args: vec![None] };
        assert_eq!(lookup_keys(&f, &|_| true).unwrap_err().text(), "SEARCH_EXPR Missing (or badly formatted) value for expr->func.args->args[ii]");
        let p = parse(b"@a + @b").unwrap();
        assert_eq!(lookup_keys(&p, &|k| k == b"a").unwrap_err().text(), "SEARCH_PROP_NOT_FOUND Property not loaded nor in pipeline: `b`");
    }

    #[test]
    fn values() {
        let trio = RsValue::Trio(Box::new([RsValue::Number(5.0), rs("[5,6]"), RsValue::Array(vec![RsValue::Number(5.0), RsValue::Number(6.0)])]));
        assert_eq!(trio.to_number(), Some(5.0));
        assert!(!trio.bool_test());
        assert_eq!(trio.compare(&RsValue::Number(4.0), false), Ok(Ordering::Greater));
        assert_eq!(trio.reply(false, false, false), Reply::bulk("[5,6]"));
        assert_eq!(RsValue::Number(1.5).reply(true, false, false), Reply::bulk("1.5"));
        assert_eq!(RsValue::Number(1.5).reply(true, true, false), Reply::Double(1.5));
        assert_eq!(RsValue::Number(3.0).reply(false, false, true), Reply::Int(3));
        assert_eq!(RsValue::Number(3.5).reply(false, false, true), Reply::Double(3.5));
        assert_eq!(RsValue::Null.reply(true, false, false), Reply::Nil);
        // Without an error sink an unconvertible string compares with the number's text.
        assert_eq!(RsValue::Number(10.0).cmp_with(&rs("9x"), None), Ordering::Less);
        let mut err = None;
        assert_eq!(RsValue::Number(10.0).cmp_with(&rs("9x"), Some(&mut err)), Ordering::Equal);
        assert_eq!(err.map(|e| e.text()), Some("Error converting string".to_string()));
        assert!(RsValue::Map(vec![]).equal(&RsValue::Map(vec![])));
        assert!(!RsValue::Number(1.0).equal(&RsValue::Array(vec![])));
        assert!(RsValue::Array(vec![]).compare(&RsValue::Array(vec![RsValue::Null]), false) == Ok(Ordering::Less));
    }

    #[test]
    fn time_conversions() {
        for t in [0i64, -1, 1700000000, -62167219200, 253402300800, 951782400, 4102444800, -2208988800, 68169600000] {
            let tm = secs_to_tm(t).unwrap();
            assert_eq!(tm_to_secs(&tm), t, "{t}");
            assert_eq!(timegm(&tm), t);
        }
        assert!(secs_to_tm(i64::MAX).is_none());
        let tm = secs_to_tm(951782400).unwrap(); // 2000-02-29
        assert_eq!((tm.year, tm.mon, tm.mday, tm.yday, tm.wday), (100, 1, 29, 59, 2));
    }
}
