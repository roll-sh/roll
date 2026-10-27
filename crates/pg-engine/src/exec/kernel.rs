//! Expressions compiled for evaluation off the executor's thread (the parallel scan's
//! workers): column references, constants, boolean logic, NULL tests, searched CASE, IN
//! lists and calls of built-in operators that read nothing but their arguments
//! (`ops::pure`), evaluated exactly as `expr::eval` evaluates them, without an `ExecCtx`.
//!
//! A kernel never reports an error itself: where `eval` would fail, or meets a value it
//! leaves to `eval` (an extension type's, a record's), it gives up (None) and the caller
//! evaluates the original expression on its own thread, which then fails as it would have.

use crate::sql::expr::Expr;
use crate::sql::func::ops::{PureFn, pure};
use crate::types::{DataType, Value};

pub enum K {
    Col(usize),
    Const(Value),
    /// A binary operator's call.
    Call {
        f: PureFn,
        args: Box<[K; 2]>,
        strict: bool,
        ret: DataType,
    },
    And(Vec<K>),
    Or(Vec<K>),
    Not(Box<K>),
    IsNull(Box<K>, bool),
    Case {
        whens: Vec<(K, K)>,
        else_: Option<Box<K>>,
    },
    /// `x [NOT] IN (…)`: each item with its `=` (called as `call2` calls it).
    InList(Box<K>, Vec<(K, PureFn)>, bool),
    /// `x op ANY / ALL (array)` (`array_op`).
    ArrayOp(Box<[K; 2]>, PureFn, bool),
    /// numeric + − × (`NumOp`): computed exactly in an i128 at the operands' scales where
    /// they fit (`num`), its intermediate values never made; `f` otherwise.
    Num {
        op: NumOp,
        f: PureFn,
        args: Box<[K; 2]>,
    },
    /// A cast that reads no session setting (`cast_pure`): a length coercion, a string type
    /// to another, a number to another, a date to a timestamp.
    Cast(Box<K>, Box<(DataType, DataType)>, bool),
}

/// A numeric operator a kernel computes without allocating (`K::Num`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumOp {
    Add,
    Sub,
    Mul,
}

/// A numeric (or integer) value as an exact unscaled i128 and its scale (None: another
/// value, or one past an i128).
fn small_num(v: &Value) -> Option<(i128, u32)> {
    use num_traits::ToPrimitive;
    match v {
        Value::Numeric(n) => match &**n {
            crate::types::numeric::Numeric::Finite { int, scale } => Some((int.to_i128()?, *scale)),
            _ => None,
        },
        Value::Int2(x) => Some((*x as i128, 0)),
        Value::Int4(x) => Some((*x as i128, 0)),
        Value::Int8(x) => Some((*x as i128, 0)),
        _ => None,
    }
}

/// Whether casting `from` to `to` reads nothing but the value (no time zone, DateStyle or
/// float output setting): `cast_value` then gives the same off the session.
fn cast_pure(from: &DataType, to: &DataType) -> bool {
    use DataType as T;
    let string = |t: &DataType| matches!(t, T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name);
    let number = |t: &DataType| matches!(t, T::Int2 | T::Int4 | T::Int8 | T::Float4 | T::Float8 | T::Numeric(_));
    if matches!(from, T::Base(_) | T::Array(_)) || matches!(to, T::Base(_) | T::Array(_)) {
        return false;
    }
    (from.same_base(to) && (string(to) || number(to) || matches!(to, T::Date | T::Timestamp(_))))
        || (string(from) && string(to))
        || (number(from) && number(to))
        || (matches!(from, T::Date) && matches!(to, T::Timestamp(_)))
}

thread_local! {
    /// The settings `cast_value` is called with by kernels (`cast_pure` casts read none).
    static FMT: crate::types::text::FormatCtx = crate::types::text::FormatCtx::default();
}

/// A row's columns as kernels read them: a row, or a joined row's two sides unjoined
/// (`pipe::Joined`).
pub trait Cols {
    fn col(&self, i: usize) -> Option<&Value>;
}

impl Cols for [Value] {
    #[inline]
    fn col(&self, i: usize) -> Option<&Value> {
        self.get(i)
    }
}

impl Cols for Vec<Value> {
    #[inline]
    fn col(&self, i: usize) -> Option<&Value> {
        self.get(i)
    }
}

fn truthy(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        _ => Some(false),
    }
}

/// A pure built-in call of `def` with result type `ty`, as `eval` would make it.
fn pure_call(def: &crate::sql::func::FuncDef, ty: &DataType) -> Option<PureFn> {
    if def.spread || def.coll.is_some() || matches!(ty, DataType::Void) {
        return None;
    }
    if let DataType::Base(b) = ty
        && b.ext != 0
    {
        return None;
    }
    pure(def.imp)
}

/// `e` as a kernel (None: it holds something kernels don't do).
pub fn compile(e: &Expr) -> Option<K> {
    Some(match e {
        Expr::Col(i, _) => K::Col(*i),
        Expr::Const(v, _) => K::Const(v.clone()),
        Expr::Relabel(e, _) | Expr::SqlSyntax(e) | Expr::Collate(e, ..) => compile(e)?,
        Expr::Cast(e, from, to, explicit) if cast_pure(from, to) => {
            K::Cast(Box::new(compile(e)?), Box::new((from.clone(), to.clone())), *explicit)
        }
        Expr::And(args) => K::And(args.iter().map(compile).collect::<Option<_>>()?),
        Expr::Or(args) => K::Or(args.iter().map(compile).collect::<Option<_>>()?),
        Expr::Not(a) => K::Not(Box::new(compile(a)?)),
        Expr::IsNull(a, neg) => K::IsNull(Box::new(compile(a)?), *neg),
        Expr::Call(def, args, ty)
            if args.len() == 2
                && def.strict
                && matches!(ty, DataType::Numeric(_))
                && let Some(op) = crate::sql::func::ops::num_op(def.imp) =>
        {
            K::Num {
                op,
                f: pure_call(def, ty)?,
                args: Box::new([compile(&args[0])?, compile(&args[1])?]),
            }
        }
        Expr::Call(def, args, ty) if args.len() == 2 => K::Call {
            f: pure_call(def, ty)?,
            args: Box::new([compile(&args[0])?, compile(&args[1])?]),
            strict: def.strict,
            ret: ty.clone(),
        },
        Expr::Case {
            operand: None,
            whens,
            else_,
            ..
        } => K::Case {
            whens: whens
                .iter()
                .map(|(w, t)| Some((compile(w)?, compile(t)?)))
                .collect::<Option<_>>()?,
            else_: match else_ {
                Some(e) => Some(Box::new(compile(e)?)),
                None => None,
            },
        },
        Expr::InList(x, list, neg) => K::InList(
            Box::new(compile(x)?),
            list.iter()
                .map(|(item, eq)| Some((compile(item)?, pure_call(eq, &DataType::Bool)?)))
                .collect::<Option<_>>()?,
            *neg,
        ),
        Expr::ArrayOp { left, right, op, all } => K::ArrayOp(
            Box::new([compile(left)?, compile(right)?]),
            pure_call(op, &DataType::Bool)?,
            *all,
        ),
        _ => return None,
    })
}

impl K {
    /// An argument's value: a column's or a constant's in place, else computed into `tmp`.
    pub fn arg<'r, R: Cols + ?Sized>(&'r self, row: &'r R, tmp: &'r mut Value) -> Option<&'r Value> {
        match self {
            K::Col(i) => row.col(*i),
            K::Const(v) => Some(v),
            k => {
                *tmp = k.run(row)?;
                Some(tmp)
            }
        }
    }

    /// A numeric expression's exact value (`K::Num` over columns, constants and other such
    /// expressions) as an unscaled i128 and its scale, computed as `Numeric`'s add, sub
    /// and mul compute it, without making a value; Some(None): NULL. None: not such an
    /// expression, or a value past an i128 (`run` then computes it).
    pub fn num<R: Cols + ?Sized>(&self, row: &R) -> Option<Option<(i128, u32)>> {
        let leaf = |v: &Value| if v.is_null() { Some(None) } else { small_num(v).map(Some) };
        match self {
            K::Col(i) => leaf(row.col(*i)?),
            K::Const(v) => leaf(v),
            K::Num { op, args, .. } => {
                // (Both operands before a NULL one: the call is strict.)
                let (a, b) = (args[0].num(row)?, args[1].num(row)?);
                let (Some((x, sx)), Some((y, sy))) = (a, b) else {
                    return Some(None);
                };
                let pow = |n: u32| 10i128.checked_pow(n);
                Some(Some(match op {
                    NumOp::Add | NumOp::Sub => {
                        let s = sx.max(sy);
                        let (x, y) = (x.checked_mul(pow(s - sx)?)?, y.checked_mul(pow(s - sy)?)?);
                        (if *op == NumOp::Add { x.checked_add(y)? } else { x.checked_sub(y)? }, s)
                    }
                    NumOp::Mul => {
                        let s = sx + sy;
                        if s as i64 > crate::types::numeric::DSCALE_MAX {
                            return None;
                        }
                        (x.checked_mul(y)?, s)
                    }
                }))
            }
            _ => None,
        }
    }

    /// The value `eval` gives over `row` (None: leave it to `eval`).
    pub fn run<R: Cols + ?Sized>(&self, row: &R) -> Option<Value> {
        Some(match self {
            K::Num { f, args, .. } => {
                if let Some(r) = self.num(row) {
                    return Some(match r {
                        None => Value::Null,
                        Some((v, s)) => Value::numeric(crate::types::numeric::Numeric::from_parts(
                            num_bigint::BigInt::from(v),
                            s,
                        )),
                    });
                }
                let (mut t0, mut t1) = (Value::Null, Value::Null);
                let x = args[0].arg(row, &mut t0)?;
                let y = args[1].arg(row, &mut t1)?;
                if x.is_null() || y.is_null() {
                    return Some(Value::Null);
                }
                if matches!(x, Value::Ext(..)) || matches!(y, Value::Ext(..)) {
                    return None;
                }
                f(x, y, &DataType::NUMERIC).ok()?
            }
            K::Col(i) => row.col(*i)?.clone(),
            K::Const(v) => v.clone(),
            K::Call { f, args, strict, ret } => {
                let (mut t0, mut t1) = (Value::Null, Value::Null);
                // (Both arguments before a strict call's NULL: the second's failure, a
                // fallback to the evaluator, still happens.)
                let x = args[0].arg(row, &mut t0)?;
                let y = args[1].arg(row, &mut t1)?;
                if *strict && (x.is_null() || y.is_null()) {
                    return Some(Value::Null);
                }
                if matches!(x, Value::Ext(..)) || matches!(y, Value::Ext(..)) {
                    return None;
                }
                f(x, y, ret).ok()?
            }
            K::And(args) => {
                let mut saw_null = false;
                for a in args {
                    match truthy(&a.run(row)?) {
                        Some(false) => return Some(Value::Bool(false)),
                        None => saw_null = true,
                        Some(true) => {}
                    }
                }
                if saw_null { Value::Null } else { Value::Bool(true) }
            }
            K::Or(args) => {
                let mut saw_null = false;
                for a in args {
                    match truthy(&a.run(row)?) {
                        Some(true) => return Some(Value::Bool(true)),
                        None => saw_null = true,
                        Some(false) => {}
                    }
                }
                if saw_null { Value::Null } else { Value::Bool(false) }
            }
            K::Not(a) => match truthy(a.arg(row, &mut Value::Null)?) {
                Some(b) => Value::Bool(!b),
                None => Value::Null,
            },
            K::IsNull(a, neg) => {
                let mut t = Value::Null;
                let v = a.arg(row, &mut t)?;
                // (A row value's test reads its fields: eval's.)
                if matches!(v, Value::Record(_)) {
                    return None;
                }
                Value::Bool(v.is_null() != *neg)
            }
            K::Case { whens, else_ } => {
                for (w, t) in whens {
                    if truthy(&w.run(row)?) == Some(true) {
                        return t.run(row);
                    }
                }
                match else_ {
                    Some(e) => e.run(row)?,
                    None => Value::Null,
                }
            }
            K::InList(x, list, neg) => {
                let x = x.run(row)?;
                if x.is_null() {
                    return Some(Value::Null);
                }
                let mut saw_null = false;
                for (item, eq) in list {
                    let mut t = Value::Null;
                    let y = item.arg(row, &mut t)?;
                    if y.is_null() {
                        saw_null = true;
                        continue;
                    }
                    if eq(&x, y, &DataType::Bool).ok()? == Value::Bool(true) {
                        return Some(Value::Bool(!*neg));
                    }
                }
                if saw_null { Value::Null } else { Value::Bool(*neg) }
            }
            K::ArrayOp(args, op, all) => {
                let (mut t0, mut t1) = (Value::Null, Value::Null);
                let x = args[0].arg(row, &mut t0)?;
                let arr = args[1].arg(row, &mut t1)?;
                if arr.is_null() {
                    return Some(Value::Null);
                }
                let Value::Array(a) = arr else { return None };
                if a.items.is_empty() {
                    return Some(Value::Bool(*all));
                }
                if x.is_null() {
                    return Some(Value::Null);
                }
                let mut saw_null = false;
                for y in &a.items {
                    if y.is_null() {
                        saw_null = true;
                        continue;
                    }
                    match (op(x, y, &DataType::Bool).ok()?, *all) {
                        (Value::Bool(true), false) => return Some(Value::Bool(true)),
                        (Value::Bool(false), true) => return Some(Value::Bool(false)),
                        (Value::Null, _) => saw_null = true,
                        _ => {}
                    }
                }
                if saw_null { Value::Null } else { Value::Bool(*all) }
            }
            K::Cast(a, types, explicit) => {
                let v = a.run(row)?;
                if v.is_null() {
                    return Some(Value::Null);
                }
                let (from, to) = &**types;
                FMT.with(|f| crate::types::cast::cast_value(v, from, to, f, *explicit)).ok()?
            }
        })
    }

    /// Whether the row passes as a filter (None: leave it to `eval`).
    pub fn passes<R: Cols + ?Sized>(&self, row: &R) -> Option<bool> {
        Some(truthy(&self.run(row)?) == Some(true))
    }
}
