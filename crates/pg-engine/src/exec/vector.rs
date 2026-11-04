//! Kernels over column batches (`storage::cols::ColBatch`): a scan's filter and an
//! aggregation's arguments compiled once from their row kernels (`kernel::K`) and run a
//! batch of rows at a time over typed vectors, with a selection of the rows still in play.
//!
//! Every kernel here gives exactly what its row kernel gives for the row, or `Tri::Out` for
//! a row it can't decide (an overflow, a value it doesn't hold): the caller then runs that
//! row through the row path, in its place, so what the row path would do (its kernel's
//! fallback, `eval`, its error) still happens at that row. Kernels are pure, so evaluating
//! one over a batch's rows before the next kernel changes nothing observable.

use super::kernel::{K, NumOp};
use crate::sql::func::ops::PureFn;
use crate::storage::cols::{Col, ColBatch, ColData};
use crate::types::{DataType, Value};
use std::cmp::Ordering;

/// A condition's outcome for a row: false, true, NULL, or `Out` (the row path decides).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    F,
    T,
    N,
    Out,
}

/// An exact number (`K::num`'s): a column, a constant, or + − × of two.
pub enum VN {
    Col(usize),
    /// None: NULL.
    Const(Option<(i128, u32)>),
    Op(NumOp, Box<[VN; 2]>),
}

/// A row's exact number as `VN::at` gives it (None: `Out`; Some(None): NULL).
pub type Num = Option<Option<(i128, u32)>>;

/// An operand of a date / timestamp comparison (`VB::Instant`): a date column or a
/// constant, as a number (None: NULL).
pub enum Inst {
    Date(usize),
    Const(Option<i64>),
}

/// An operand of a comparison: a column or a constant.
pub enum Leaf {
    Col(usize, DataType),
    Const(Value),
}

/// A condition.
pub enum VB {
    And(Vec<VB>),
    Or(Vec<VB>),
    Not(Box<VB>),
    /// `col IS [NOT] NULL`.
    IsNull(usize, bool),
    /// A boolean column.
    Col(usize),
    /// `K::NumCmp` over exact numbers; `f` where they don't fit (the row path then).
    NumCmp { test: fn(Ordering) -> bool, args: Box<[VN; 2]> },
    /// A string comparison of bytes (`ops::str_cmp`): column against constant, bpchar's
    /// without trailing spaces (the constant trimmed already).
    Str { test: fn(Ordering) -> bool, col: usize, k: Box<[u8]>, bp: bool, col_first: bool },
    /// `col [NOT] IN (strings)` by bytes (each item's `=` a byte comparison), and
    /// `col = ANY (array)` / `col <> ALL (array)` of strings.
    StrIn { col: usize, items: Vec<Option<Box<[u8]>>>, bp: bool, neg: bool },
    /// `col [NOT] LIKE 'pattern'` of a text column (`K::Like`).
    Like { col: usize, pat: Box<crate::sql::func::string::LikePattern>, neg: bool },
    /// `col = ANY (array)` / `col <> ALL (array)` of exact numbers (None: a NULL item).
    NumIn { col: usize, items: Vec<Option<(i128, u32)>>, neg: bool },
    /// A comparison of dates, or of dates and timestamps (`ts`: as `ops::p_instant`s, a date
    /// its timestamp; else a date's day number), strict: as integers.
    Instant { test: fn(Ordering) -> bool, args: Box<[Inst; 2]>, ts: bool },
    /// Any other pure binary call returning bool over fixed-width columns and constants:
    /// each column's value made (no allocation) and `f` called, as `K::Call` calls it.
    Call { f: PureFn, args: Box<[Leaf; 2]>, strict: bool },
}

impl VN {
    pub fn compile(k: &K) -> Option<VN> {
        Some(match k {
            K::Col(i) => VN::Col(*i),
            K::Const(v) if v.is_null() => VN::Const(None),
            K::Const(v) => VN::Const(Some(super::kernel::small_num(v)?)),
            K::Num { op, args, .. } => VN::Op(*op, Box::new([VN::compile(&args[0])?, VN::compile(&args[1])?])),
            _ => return None,
        })
    }

    /// Row `r`'s number (Some(None): NULL; None: not one, or past an i128: `Out`).
    #[inline]
    pub fn at(&self, b: &ColBatch, r: usize) -> Option<Option<(i128, u32)>> {
        match self {
            VN::Col(i) => num_at(col_at(b, *i)?, r),
            VN::Const(c) => Some(*c),
            VN::Op(op, args) => {
                // (Both operands before a NULL one: `K::num`'s order.)
                let (a, b) = (args[0].at(b, r)?, args[1].at(b, r)?);
                let (Some((x, sx)), Some((y, sy))) = (a, b) else {
                    return Some(None);
                };
                Some(Some(num_op(*op, x, sx, y, sy)?))
            }
        }
    }
}

impl VN {
    /// Each row of `sel`'s number into `out` (by row: `at`'s), a column at a time.
    pub fn eval(&self, b: &ColBatch, sel: &[u32], out: &mut [Num]) {
        match self {
            VN::Col(i) => {
                let Some(c) = col_at(b, *i) else {
                    return sel.iter().for_each(|&r| out[r as usize] = None);
                };
                macro_rules! each {
                    ($v:expr, $f:expr) => {
                        for &r in sel {
                            let r = r as usize;
                            out[r] = Some(if c.nulls[r] { None } else { Some($f($v[r])) });
                        }
                    };
                }
                match &c.data {
                    ColData::I32(v) => each!(v, |x: i32| (x as i128, 0)),
                    ColData::I64(v) => each!(v, |x: i64| (x as i128, 0)),
                    ColData::Num(v) => each!(v, |x: (i128, u32)| x),
                    _ => sel.iter().for_each(|&r| out[r as usize] = None),
                }
            }
            VN::Const(k) => sel.iter().for_each(|&r| out[r as usize] = Some(*k)),
            VN::Op(op, args) => {
                args[0].eval(b, sel, out);
                let mut y = vec![None; out.len()];
                args[1].eval(b, sel, &mut y);
                for &r in sel {
                    let r = r as usize;
                    // (Both operands before a NULL one: `K::num`'s order.)
                    out[r] = match (out[r], y[r]) {
                        (Some(Some((x, sx))), Some(Some((y, sy)))) => num_op(*op, x, sx, y, sy).map(Some),
                        (Some(_), Some(_)) => Some(None),
                        _ => None,
                    };
                }
            }
        }
    }
}

impl Inst {
    fn compile(k: &K, types: &[DataType], ts: bool) -> Option<Inst> {
        Some(match k {
            K::Col(i) if matches!(types.get(*i), Some(DataType::Date)) => Inst::Date(*i),
            K::Const(Value::Null) => Inst::Const(None),
            K::Const(Value::Date(d)) => Inst::Const(Some(if ts { crate::sql::func::ops::date_instant(*d) } else { *d as i64 })),
            K::Const(Value::Timestamp(t)) if ts => Inst::Const(Some(*t)),
            _ => return None,
        })
    }

    /// Row `r`'s number (Some(None): NULL; None: not a date column).
    #[inline]
    fn at(&self, b: &ColBatch, r: usize, ts: bool) -> Option<Option<i64>> {
        match self {
            Inst::Const(k) => Some(*k),
            Inst::Date(i) => {
                let c = col_at(b, *i)?;
                let ColData::Date(v) = &c.data else { return None };
                Some((!c.nulls[r]).then(|| if ts { crate::sql::func::ops::date_instant(v[r]) } else { v[r] as i64 }))
            }
        }
    }
}

/// `K::num`'s + − × of two exact numbers (None: past an i128, or past numeric's scale).
#[inline]
fn num_op(op: NumOp, x: i128, sx: u32, y: i128, sy: u32) -> Option<(i128, u32)> {
    let pow = |n: u32| 10i128.checked_pow(n);
    Some(match op {
        NumOp::Add | NumOp::Sub => {
            let s = sx.max(sy);
            let (x, y) = if sx == sy { (x, y) } else { (x.checked_mul(pow(s - sx)?)?, y.checked_mul(pow(s - sy)?)?) };
            (if op == NumOp::Add { x.checked_add(y)? } else { x.checked_sub(y)? }, s)
        }
        NumOp::Mul => {
            let s = sx + sy;
            if s as i64 > crate::types::numeric::DSCALE_MAX {
                return None;
            }
            (x.checked_mul(y)?, s)
        }
    })
}

/// Table column `i` of the batch (None: not decoded).
#[inline]
fn col_at(b: &ColBatch, i: usize) -> Option<&Col> {
    b.cols.get(i)?.as_ref()
}

/// Column `c`'s row `r` as an exact number (`Cols::num`'s).
#[inline]
fn num_at(c: &Col, r: usize) -> Option<Option<(i128, u32)>> {
    if c.nulls[r] {
        return Some(None);
    }
    Some(Some(match &c.data {
        ColData::I32(v) => (v[r] as i128, 0),
        ColData::I64(v) => (v[r] as i128, 0),
        ColData::Num(v) => v[r],
        _ => return None,
    }))
}

/// Column `c`'s row `r` (of type `ty`) as the value the row path reads (None: one this
/// doesn't make without allocating: a string, a numeric).
#[inline]
pub fn value_at(c: &Col, r: usize, ty: &DataType) -> Option<Value> {
    if c.nulls[r] {
        return Some(Value::Null);
    }
    Some(match &c.data {
        ColData::I32(v) if matches!(ty, DataType::Int2) => Value::Int2(v[r] as i16),
        ColData::I32(v) => Value::Int4(v[r]),
        ColData::I64(v) => Value::Int8(v[r]),
        ColData::F64(v) if matches!(ty, DataType::Float4) => Value::Float4(v[r] as f32),
        ColData::F64(v) => Value::Float8(v[r]),
        ColData::Bool(v) => Value::Bool(v[r]),
        ColData::Date(v) => Value::Date(v[r]),
        ColData::Values(v) if plain_const(&v[r]) => v[r].clone(),
        _ => return None,
    })
}

/// Whether a constant can stand in a `VB::Call` (a value made without allocating, or one
/// held: the constant is borrowed).
fn plain_const(v: &Value) -> bool {
    !matches!(v, Value::Ext(..) | Value::Record(_))
}

impl Leaf {
    fn compile(k: &K, types: &[DataType]) -> Option<Leaf> {
        match k {
            K::Col(i) => Some(Leaf::Col(*i, types.get(*i)?.clone())),
            K::Const(v) if plain_const(v) => Some(Leaf::Const(v.clone())),
            _ => None,
        }
    }
}

/// A string constant's bytes for a comparison (bpchar's trimmed; None inside: NULL).
fn text_const(v: &Value, bp: bool) -> Option<Option<Box<[u8]>>> {
    match v {
        Value::Null => Some(None),
        Value::Text(s) if bp => Some(Some(crate::types::text::bpchar_trim(s).as_bytes().into())),
        Value::Text(s) => Some(Some(s.as_bytes().into())),
        _ => None,
    }
}

/// Whether column `i` is a string the row path reads as `Value::Text` (its stored bytes
/// compared: `ops::str_cmp`).
fn text_col(types: &[DataType], i: usize) -> bool {
    matches!(types.get(i), Some(DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_)))
}

/// A string column's row `r` as compared (bpchar's without trailing spaces; None: not a
/// string).
#[inline]
fn text_at<'b>(c: &Col, b: &'b ColBatch, r: usize, bp: bool) -> Option<&'b [u8]> {
    if !matches!(c.data, ColData::Bytes(_)) {
        return None;
    }
    let mut x = c.bytes(b, r);
    if bp {
        while let [rest @ .., b' '] = x {
            x = rest;
        }
    }
    Some(x)
}

/// What `truthy` makes of a call's value.
#[inline]
fn tri_of(v: &Value) -> Tri {
    match v {
        Value::Null => Tri::N,
        Value::Bool(true) => Tri::T,
        _ => Tri::F,
    }
}

#[inline]
fn fill(sel: &[u32], out: &mut [Tri], t: Tri) {
    for &r in sel {
        out[r as usize] = t;
    }
}

/// Two exact numbers compared at the larger scale (None: past an i128: `K::NumCmp` then
/// calls its function).
#[inline]
fn scaled_cmp(x: i128, sx: u32, y: i128, sy: u32) -> Option<Ordering> {
    if sx == sy {
        return Some(x.cmp(&y));
    }
    let s = sx.max(sy);
    let pow = |n: u32| 10i128.checked_pow(n);
    Some(x.checked_mul(pow(s - sx)?)?.cmp(&y.checked_mul(pow(s - sy)?)?))
}

impl VB {
    pub fn compile(k: &K, types: &[DataType]) -> Option<VB> {
        use crate::sql::func::ops::str_cmp;
        Some(match k {
            K::And(a) => VB::And(a.iter().map(|a| VB::compile(a, types)).collect::<Option<_>>()?),
            K::Or(a) => VB::Or(a.iter().map(|a| VB::compile(a, types)).collect::<Option<_>>()?),
            K::Not(a) => VB::Not(Box::new(VB::compile(a, types)?)),
            K::IsNull(a, neg) => match **a {
                K::Col(i) if i < types.len() => VB::IsNull(i, *neg),
                _ => return None,
            },
            K::Col(i) if matches!(types.get(*i), Some(DataType::Bool)) => VB::Col(*i),
            K::NumCmp { test, args, .. } => {
                VB::NumCmp { test: *test, args: Box::new([VN::compile(&args[0])?, VN::compile(&args[1])?]) }
            }
            K::Call { f, args, strict, ret: DataType::Bool } => {
                let str_arg = match &**args {
                    [K::Col(i), K::Const(c @ Value::Text(_))] if text_col(types, *i) => Some((*i, c, true)),
                    [K::Const(c @ Value::Text(_)), K::Col(i)] if text_col(types, *i) => Some((*i, c, false)),
                    _ => None,
                };
                if let (Some((test, bp)), Some((col, k, col_first)), true) = (str_cmp(*f), str_arg, *strict) {
                    let k = text_const(k, bp)??;
                    return Some(VB::Str { test, col, k, bp, col_first });
                }
                let inst = crate::sql::func::ops::dt_cmp(*f)
                    .map(|t| (t, true))
                    .or_else(|| str_cmp(*f).filter(|(_, bp)| !bp).map(|(t, _)| (t, false)));
                if let (Some((test, ts)), true) = (inst, *strict)
                    && let (Some(x), Some(y)) = (Inst::compile(&args[0], types, ts), Inst::compile(&args[1], types, ts))
                    && (matches!(x, Inst::Date(_)) || matches!(y, Inst::Date(_)))
                {
                    return Some(VB::Instant { test, args: Box::new([x, y]), ts });
                }
                VB::Call { f: *f, args: Box::new([Leaf::compile(&args[0], types)?, Leaf::compile(&args[1], types)?]), strict: *strict }
            }
            K::InList(x, list, neg) => {
                let K::Col(col) = **x else { return None };
                if !text_col(types, col) {
                    return None;
                }
                let mut bp = None;
                let mut items = Vec::with_capacity(list.len());
                for (item, eq) in list {
                    let (test, b) = str_cmp(*eq)?;
                    let is_eq = test(Ordering::Equal) && !test(Ordering::Less) && !test(Ordering::Greater);
                    if !is_eq || *bp.get_or_insert(b) != b {
                        return None;
                    }
                    let K::Const(v) = item else { return None };
                    items.push(text_const(v, b)?);
                }
                VB::StrIn { col, items, bp: bp?, neg: *neg }
            }
            K::Like(a, pat, neg) => match **a {
                K::Col(col) if matches!(types.get(col), Some(DataType::Text | DataType::Varchar(_))) => {
                    VB::Like { col, pat: pat.clone(), neg: *neg }
                }
                _ => return None,
            },
            K::ArrayOp(args, f, all) => {
                // (An empty array is false for ANY even for a NULL: left to the row path.)
                let [K::Col(col), K::Const(Value::Array(a))] = &**args else { return None };
                if a.items.is_empty() {
                    return None;
                }
                let (test, bp) = str_cmp(*f)?;
                let eq = test(Ordering::Equal) && !test(Ordering::Less) && !test(Ordering::Greater);
                let ne = !test(Ordering::Equal) && test(Ordering::Less) && test(Ordering::Greater);
                let neg = match (*all, eq, ne) {
                    (false, true, _) => false,
                    (true, _, true) => true,
                    _ => return None,
                };
                if text_col(types, *col) {
                    let items = a.items.iter().map(|v| text_const(v, bp)).collect::<Option<_>>()?;
                    VB::StrIn { col: *col, items, bp, neg }
                } else if !bp && matches!(types.get(*col), Some(DataType::Int2 | DataType::Int4 | DataType::Int8 | DataType::Numeric(_))) {
                    let num = |v: &Value| if v.is_null() { Some(None) } else { super::kernel::small_num(v).map(Some) };
                    VB::NumIn { col: *col, items: a.items.iter().map(num).collect::<Option<_>>()?, neg }
                } else {
                    return None;
                }
            }
            _ => return None,
        })
    }

    /// The condition over the rows `sel` of `b`, into `out` (indexed by row).
    pub fn eval(&self, b: &ColBatch, sel: &[u32], out: &mut [Tri]) {
        match self {
            VB::And(args) | VB::Or(args) => {
                let and = matches!(self, VB::And(_));
                // (The rows no argument decided yet; `out` holds N for those one was NULL
                // for.)
                let mut live: Vec<u32> = sel.to_vec();
                fill(sel, out, Tri::T);
                let mut tmp = vec![Tri::Out; b.len];
                for a in args {
                    if live.is_empty() {
                        break;
                    }
                    a.eval(b, &live, &mut tmp);
                    live.retain(|&r| {
                        let r = r as usize;
                        match (tmp[r], and) {
                            (Tri::F, true) | (Tri::T, false) | (Tri::Out, _) => {
                                out[r] = tmp[r];
                                false
                            }
                            (Tri::N, _) => {
                                out[r] = Tri::N;
                                true
                            }
                            _ => true,
                        }
                    });
                }
                // (Undecided: true for AND, false for OR, unless an argument was NULL.)
                for r in live {
                    let r = r as usize;
                    if out[r] != Tri::N {
                        out[r] = if and { Tri::T } else { Tri::F };
                    }
                }
            }
            VB::Not(a) => {
                a.eval(b, sel, out);
                for &r in sel {
                    let o = &mut out[r as usize];
                    *o = match *o {
                        Tri::T => Tri::F,
                        Tri::F => Tri::T,
                        x => x,
                    };
                }
            }
            VB::IsNull(c, neg) => {
                let Some(c) = col_at(b, *c) else { return fill(sel, out, Tri::Out) };
                for &r in sel {
                    out[r as usize] = if c.nulls[r as usize] != *neg { Tri::T } else { Tri::F };
                }
            }
            VB::Col(c) => {
                let Some(c) = col_at(b, *c) else { return fill(sel, out, Tri::Out) };
                for &r in sel {
                    let r = r as usize;
                    out[r] = match (&c.data, c.nulls[r]) {
                        (_, true) => Tri::N,
                        (ColData::Bool(v), _) if v[r] => Tri::T,
                        _ => Tri::F,
                    };
                }
            }
            VB::NumCmp { test, args } => {
                let mut x = vec![None; b.len];
                let mut y = vec![None; b.len];
                args[0].eval(b, sel, &mut x);
                args[1].eval(b, sel, &mut y);
                for &r in sel {
                    let r = r as usize;
                    out[r] = match (x[r], y[r]) {
                        (Some(Some((x, sx))), Some(Some((y, sy)))) => match scaled_cmp(x, sx, y, sy) {
                            Some(o) if test(o) => Tri::T,
                            Some(_) => Tri::F,
                            None => Tri::Out,
                        },
                        (Some(_), Some(_)) => Tri::N,
                        _ => Tri::Out,
                    };
                }
            }
            VB::Str { test, col, k, bp, col_first } => {
                let Some(c) = col_at(b, *col) else { return fill(sel, out, Tri::Out) };
                for &r in sel {
                    let r = r as usize;
                    out[r] = match (c.nulls[r], text_at(c, b, r, *bp)) {
                        (true, _) => Tri::N,
                        (false, Some(x)) => {
                            let o = x.cmp(k);
                            if test(if *col_first { o } else { o.reverse() }) { Tri::T } else { Tri::F }
                        }
                        (false, None) => Tri::Out,
                    };
                }
            }
            VB::StrIn { col, items, bp, neg } => {
                let Some(c) = col_at(b, *col) else { return fill(sel, out, Tri::Out) };
                let saw_null = items.iter().any(Option::is_none);
                let (hit, miss) = if *neg { (Tri::F, Tri::T) } else { (Tri::T, Tri::F) };
                for &r in sel {
                    let r = r as usize;
                    out[r] = match (c.nulls[r], text_at(c, b, r, *bp)) {
                        (true, _) => Tri::N,
                        (false, Some(x)) if items.iter().flatten().any(|i| **i == *x) => hit,
                        (false, Some(_)) if saw_null => Tri::N,
                        (false, Some(_)) => miss,
                        (false, None) => Tri::Out,
                    };
                }
            }
            VB::Like { col, pat, neg } => {
                let Some(c) = col_at(b, *col) else { return fill(sel, out, Tri::Out) };
                if !matches!(c.data, ColData::Bytes(_)) {
                    return fill(sel, out, Tri::Out);
                }
                for &r in sel {
                    let r = r as usize;
                    out[r] = if c.nulls[r] {
                        Tri::N
                    } else {
                        match std::str::from_utf8(c.bytes(b, r)).ok().map(|s| pat.matches(s)) {
                            Some(Ok(m)) if m != *neg => Tri::T,
                            Some(Ok(_)) => Tri::F,
                            _ => Tri::Out,
                        }
                    };
                }
            }
            VB::NumIn { col, items, neg } => {
                let Some(c) = col_at(b, *col) else { return fill(sel, out, Tri::Out) };
                let saw_null = items.iter().any(Option::is_none);
                let (hit, miss) = if *neg { (Tri::F, Tri::T) } else { (Tri::T, Tri::F) };
                for &r in sel {
                    let r = r as usize;
                    out[r] = match num_at(c, r) {
                        None => Tri::Out,
                        Some(None) => Tri::N,
                        Some(Some((x, sx))) => {
                            let mut t = if saw_null { Tri::N } else { miss };
                            for &(y, sy) in items.iter().flatten() {
                                match scaled_cmp(x, sx, y, sy) {
                                    Some(Ordering::Equal) => {
                                        t = hit;
                                        break;
                                    }
                                    Some(_) => {}
                                    None => t = Tri::Out,
                                }
                            }
                            t
                        }
                    };
                }
            }
            VB::Instant { test, args, ts } => {
                for &r in sel {
                    let r = r as usize;
                    out[r] = match (args[0].at(b, r, *ts), args[1].at(b, r, *ts)) {
                        (Some(Some(x)), Some(Some(y))) if test(x.cmp(&y)) => Tri::T,
                        (Some(Some(_)), Some(Some(_))) => Tri::F,
                        (Some(_), Some(_)) => Tri::N,
                        _ => Tri::Out,
                    };
                }
            }
            VB::Call { f, args, strict } => {
                for &r in sel {
                    let r = r as usize;
                    fn leaf<'a>(l: &'a Leaf, b: &ColBatch, r: usize) -> Option<std::borrow::Cow<'a, Value>> {
                        match l {
                            Leaf::Col(i, ty) => value_at(col_at(b, *i)?, r, ty).map(std::borrow::Cow::Owned),
                            Leaf::Const(v) => Some(std::borrow::Cow::Borrowed(v)),
                        }
                    }
                    out[r] = match (leaf(&args[0], b, r), leaf(&args[1], b, r)) {
                        (Some(x), Some(y)) if *strict && (x.is_null() || y.is_null()) => Tri::N,
                        (Some(x), Some(y)) => match f(&x, &y, &DataType::Bool) {
                            Ok(v) => tri_of(&v),
                            Err(_) => Tri::Out,
                        },
                        _ => Tri::Out,
                    };
                }
            }
        }
    }

    /// The columns the condition reads.
    fn cols(&self, out: &mut Vec<usize>) {
        match self {
            VB::And(a) | VB::Or(a) => a.iter().for_each(|a| a.cols(out)),
            VB::Not(a) => a.cols(out),
            VB::IsNull(c, _) | VB::Col(c) | VB::Str { col: c, .. } | VB::StrIn { col: c, .. } | VB::NumIn { col: c, .. } | VB::Like { col: c, .. } => out.push(*c),
            VB::NumCmp { args, .. } => args.iter().for_each(|a| a.cols(out)),
            VB::Instant { args, .. } => out.extend(args.iter().filter_map(|a| match a {
                Inst::Date(c) => Some(*c),
                Inst::Const(_) => None,
            })),
            VB::Call { args, .. } => out.extend(args.iter().filter_map(|l| match l {
                Leaf::Col(c, _) => Some(*c),
                Leaf::Const(_) => None,
            })),
        }
    }
}

impl VN {
    fn cols(&self, out: &mut Vec<usize>) {
        match self {
            VN::Col(c) => out.push(*c),
            VN::Const(_) => {}
            VN::Op(_, a) => a.iter().for_each(|a| a.cols(out)),
        }
    }
}

/// An aggregate's argument as a batch reads it.
pub enum AggArg {
    /// count(*).
    Star,
    /// count(col): whether it is NULL.
    Count(usize),
    /// A sum's or average's exact number.
    Num(VN),
}

/// A scan's filter and partial aggregation (`partial::Spec`) over column batches: the
/// filter (None: none), the group key columns, each aggregate's argument, and the columns
/// decoded (a mask over the table's). Or, below a hash join's probe (`probe`: its integer
/// key column, `pipe::Probe::int_key_col`), the filter and the probe's lookup only: a row
/// that passes and has candidates goes through the row path, the others are dropped.
pub struct AggPlan {
    pub filter: Option<VB>,
    pub keys: Vec<usize>,
    /// The key columns' types.
    pub key_types: Vec<DataType>,
    pub args: Vec<AggArg>,
    pub need: Vec<bool>,
    pub probe: Option<usize>,
}

impl AggPlan {
    /// The plan for a scan of columns `types` filtered by `filter` (`filtered`: it has a
    /// filter, which may be the executor thread's) aggregating by `spec` (or probing by
    /// `probe`); None where some part has no batch kernel (the row path does it all then).
    pub fn compile(
        types: &[DataType],
        filter: Option<&K>,
        filtered: bool,
        spec: Option<&super::partial::Spec>,
        probe: Option<usize>,
    ) -> Option<AggPlan> {
        use super::partial::Partial;
        let filter = match (filter, filtered) {
            (Some(f), true) => Some(VB::compile(f, types)?),
            (None, false) => None,
            _ => return None,
        };
        let (mut keys, mut args) = (Vec::new(), Vec::new());
        if let (None, Some(spec)) = (probe, spec) {
            keys = spec
                .keys
                .iter()
                .map(|k| match k {
                    K::Col(c) => Some(*c),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            for (a, _, p) in &spec.aggs {
                args.push(match (a, p) {
                    (None, Partial::Count(_)) => AggArg::Star,
                    (Some(K::Col(c)), Partial::Count(_)) => AggArg::Count(*c),
                    (Some(k), Partial::SumInt(_) | Partial::AvgInt(..) | Partial::SumNum(_)) => AggArg::Num(VN::compile(k)?),
                    _ => return None,
                });
            }
        }
        let mut cols = keys.clone();
        cols.extend(probe);
        if let Some(f) = &filter {
            f.cols(&mut cols);
        }
        for a in &args {
            match a {
                AggArg::Star => {}
                AggArg::Count(c) => cols.push(*c),
                AggArg::Num(n) => n.cols(&mut cols),
            }
        }
        // (The row's key column, after the table's, isn't decoded into a batch.)
        if cols.iter().any(|c| *c >= types.len()) {
            return None;
        }
        let mut need = vec![false; types.len()];
        for c in cols {
            need[c] = true;
        }
        let key_types = keys.iter().map(|c| types[*c].clone()).collect();
        Some(AggPlan { filter, keys, key_types, args, need, probe })
    }

    /// Whether row `r` of `b` misses the probe's table (its key NULL or not one of the
    /// table's): no row comes of it. False where it may match, or the batch can't tell.
    #[inline]
    pub fn misses(&self, probe: &super::pipe::Probe, b: &ColBatch, r: usize) -> bool {
        let Some(c) = self.probe.and_then(|c| col_at(b, c)) else { return false };
        if c.nulls[r] {
            return true;
        }
        let x = match &c.data {
            ColData::I32(v) => v[r] as i64,
            ColData::I64(v) => v[r],
            _ => return false,
        };
        probe.built.int_span(x).is_some_and(|s| s.1 == 0)
    }
}
