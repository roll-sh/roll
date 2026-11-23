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
    /// `x [NOT] IN (…)` of an integer `x` and integer constants under the built-in `=`
    /// (int2, int4, int8 in any mix: equal as numbers): the items sorted and each once,
    /// whether one is NULL, NOT. (An IN sub-select's rows put in as a list, TPC-H q16's
    /// `ps_suppkey NOT IN (…)`: a row tested without an `=` call per item.)
    IntIn(Box<K>, Box<[i64]>, bool, bool),
    /// `x op ANY / ALL (array)` (`array_op`).
    ArrayOp(Box<[K; 2]>, PureFn, bool),
    /// numeric + − × (`NumOp`): computed exactly in an i128 at the operands' scales where
    /// they fit (`num`), its intermediate values never made; `f` otherwise.
    Num {
        op: NumOp,
        f: PureFn,
        args: Box<[K; 2]>,
    },
    /// integer + − × % (`IntOp`) of integer types: computed exactly where both operands are
    /// numbers (`num`: a column read in place is never made a value), the result in its
    /// type's range; `f` otherwise (NULL, a zero divisor, an overflow: `eval` reports it).
    Int {
        op: IntOp,
        f: PureFn,
        args: Box<[K; 2]>,
        ret: DataType,
    },
    /// A cast that reads no session setting (`cast_pure`): a length coercion, a string type
    /// to another, a number to another, a date to a timestamp.
    Cast(Box<K>, Box<(DataType, DataType)>, bool),
    /// `text [NOT] LIKE 'pattern'` (`neg`: NOT), the pattern compiled once, under no
    /// collation or a deterministic one (which LIKE doesn't consult).
    Like(Box<K>, Box<crate::sql::func::string::LikePattern>, bool),
    /// A pgvector distance function's call (strict, float8): its kernel, when the values'
    /// dimensions agree (`ext::vector::distance_kernel`).
    Dist(crate::ext::vector::DistKernel, Box<[K; 2]>),
    /// A comparison of two numerics: as exact numbers (`num`) where both fit an i128, so a
    /// column read in place (`Cols::num`) is never made a value; `f` otherwise.
    NumCmp {
        test: fn(std::cmp::Ordering) -> bool,
        f: PureFn,
        args: Box<[K; 2]>,
    },
    /// `substring(text FROM start FOR count)` of constants, the count not negative
    /// (`string::substr_chars`; TPC-H q22's country codes).
    Substr(Box<K>, i64, i64),
}

/// A numeric operator a kernel computes without allocating (`K::Num`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumOp {
    Add,
    Sub,
    Mul,
}

/// An integer operator a kernel computes without making values (`K::Int`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IntOp {
    Add,
    Sub,
    Mul,
    Mod,
}

impl IntOp {
    /// `x op y` of integers (scale 0) as PostgreSQL computes it into `ret` (int2, int4 or
    /// int8): None where it raises an error (a result past the type's range, a zero
    /// divisor) or an operand isn't an integer. (`%` in 64 bits, as int8mod: an i128
    /// remainder is a library call; int8 minimum % -1 is 0.)
    #[inline]
    pub fn apply(self, (x, sx): (i128, u32), (y, sy): (i128, u32), ret: &DataType) -> Option<i128> {
        if sx != 0 || sy != 0 {
            return None;
        }
        let r = match self {
            IntOp::Add => x.checked_add(y)?,
            IntOp::Sub => x.checked_sub(y)?,
            IntOp::Mul => x.checked_mul(y)?,
            IntOp::Mod => {
                let (x, y) = (i64::try_from(x).ok()?, i64::try_from(y).ok()?);
                if y == 0 {
                    return None;
                }
                x.wrapping_rem(y) as i128
            }
        };
        let fits = match ret {
            DataType::Int2 => i16::try_from(r).is_ok(),
            DataType::Int4 => i32::try_from(r).is_ok(),
            DataType::Int8 => i64::try_from(r).is_ok(),
            _ => false,
        };
        fits.then_some(r)
    }
}

/// An integer `r` (`IntOp::apply`'s, in `ret`'s range) as a value of type `ret`.
fn int_value(r: i128, ret: &DataType) -> Value {
    match ret {
        DataType::Int2 => Value::Int2(r as i16),
        DataType::Int4 => Value::Int4(r as i32),
        _ => Value::Int8(r as i64),
    }
}

/// A numeric (or integer) value as an exact unscaled i128 and its scale (None: another
/// value, or one past an i128).
/// 10^n for n <= 38 (10^39 is past an i128).
const POW10: [i128; 39] = {
    let mut t = [1i128; 39];
    let mut i = 1;
    while i < 39 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

/// `x * y` exactly (None: past an i128): one widening multiply when both fit an i64 (their
/// product can't leave an i128), else `checked_mul` (a call to the 128-bit overflow
/// multiply).
#[inline]
pub(super) fn mul_exact(x: i128, y: i128) -> Option<i128> {
    match (i64::try_from(x), i64::try_from(y)) {
        (Ok(a), Ok(b)) => Some(a as i128 * b as i128),
        _ => x.checked_mul(y),
    }
}

/// `x * 10^n` exactly (None: past an i128), as `x.checked_mul(10i128.checked_pow(n)?)`.
#[inline]
pub(super) fn scale_up(x: i128, n: u32) -> Option<i128> {
    match n {
        0 => Some(x),
        _ => mul_exact(x, *POW10.get(n as usize)?),
    }
}

/// `K::num`'s + − × of two exact numbers at their scales (None: past an i128, or a product
/// past numeric's scale).
#[inline]
pub(super) fn num_op(op: NumOp, x: i128, sx: u32, y: i128, sy: u32) -> Option<(i128, u32)> {
    Some(match op {
        NumOp::Add | NumOp::Sub => {
            let s = sx.max(sy);
            let (x, y) = (scale_up(x, s - sx)?, scale_up(y, s - sy)?);
            (if op == NumOp::Add { x.checked_add(y)? } else { x.checked_sub(y)? }, s)
        }
        NumOp::Mul => {
            let s = sx + sy;
            if s as i64 > crate::types::numeric::DSCALE_MAX {
                return None;
            }
            (mul_exact(x, y)?, s)
        }
    })
}

pub(super) fn small_num(v: &Value) -> Option<(i128, u32)> {
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

    /// Column `i` as an exact number (`small_num`; Some(None): NULL; None: absent, or not
    /// such a number). A stored row read in place gives it without making a value.
    fn num(&self, i: usize) -> Option<Option<(i128, u32)>> {
        let v = self.col(i)?;
        if v.is_null() { Some(None) } else { small_num(v).map(Some) }
    }

    /// Column `i` as a string (Some(None): NULL; None: absent, or not a string). A stored
    /// row read in place gives it without making a value.
    fn text(&self, i: usize) -> Option<Option<&str>> {
        match self.col(i)? {
            Value::Null => Some(None),
            Value::Text(s) => Some(Some(s)),
            _ => None,
        }
    }
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

/// A string comparison (=, <>, <, <=, >, >=, bpchar's) called under a collation in which it
/// compares as the strings' bytes compare: one that sorts bytewise (C, POSIX, a libc locale
/// here, the builtin provider's: `Collation::cmp` compares bytes), or, for = and <>, any
/// deterministic one (`Collation::eq`: equal strings are equal bytes). Its pure call, which
/// compares bytes.
fn collated_bytes(def: &crate::sql::func::FuncDef, args: &[Expr]) -> Option<PureFn> {
    let c = def.coll.as_ref()?;
    let string = |t: &DataType| matches!(t, DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_) | DataType::Name);
    if def.spread || !args.iter().all(|a| string(&a.ty())) || crate::catalog::partition::comparator_fn(c).is_some() {
        return None;
    }
    let f = pure(def.imp)?;
    crate::sql::func::ops::str_cmp(f)?;
    (c.bytewise() || (c.deterministic && crate::sql::func::ops::str_eq_op(def.imp))).then_some(f)
}

/// A filter of comparisons, one or ANDed, of a column with a constant, with a column of the
/// outer row of a nested loop (an inner scan's `^1#k`, `of_joined`) or with another column of
/// the row, run over a stored row's encoded values in place: the columns walked once, nothing
/// decoded, made or cached.
/// Numeric columns (`K::NumCmp`) compare as exact numbers (`row::peek_num`), string ones
/// (`K::Call` of a string comparison that compares bytes: `ops::str_cmp`, `collated_bytes`)
/// as their stored bytes (`row::peek_text`), bpchar's without trailing spaces, dates and
/// timestamps (`row::peek_dt`) as their numbers or, for a date / timestamp comparison
/// (`ops::dt_cmp`), as instants. What a row's
/// values don't decide this way (a column added since it was stored, a number past an
/// i128, a value stored out of line, an outer value of another type) is left to the kernel.
pub struct StoredCmps {
    cmps: Vec<StoredCmp>,
    /// The columns walked: up to the last one compared.
    upto: usize,
    /// A comparison of two of the row's columns: the walk keeps the columns' places.
    pairs: bool,
    /// No integer operator: no comparison can raise an error, so the first false one
    /// decides the row.
    exact: bool,
}

/// One comparison of `StoredCmps`.
struct StoredCmp {
    col: usize,
    /// The integer operator the column goes through first (`id % 4 = 1`): the operator, its
    /// constant operand, whether the column is that one's first operand, the result type.
    op: Option<(IntOp, (i128, u32), bool, DataType)>,
    how: How,
    other: Other,
    test: fn(std::cmp::Ordering) -> bool,
    /// Whether the column is the comparison's first argument.
    col_first: bool,
}

/// How a comparison's operands compare.
#[derive(Clone, Copy)]
enum How {
    /// As exact numbers (`K::NumCmp`).
    Num,
    /// As strings' bytes (bpchar's without trailing spaces when true), or dates and
    /// timestamps of one type as their numbers (`ops::str_cmp`).
    Str(bool),
    /// As instants (`ops::dt_cmp`).
    Instant,
}

/// What a stored value is compared with.
enum Other {
    Const(Against),
    /// The outer row's column.
    Outer(usize),
    /// Another column of the stored row.
    Col(usize),
}

/// The constant a stored value is compared with.
enum Against {
    Num((i128, u32)),
    /// A string's bytes; bpchar's (trailing spaces not compared) when true, the constant's
    /// trimmed already.
    Text(Box<[u8]>, bool),
    /// A date or a timestamp of a comparison of two of the same type (`cmp_total`: as its
    /// number; a stored value of another type is left to the kernel).
    Date(i32),
    Ts(i64),
    /// A date or a timestamp of a date / timestamp comparison (`p_dt_*`: both sides as
    /// instants, `ops::date_instant`).
    Instant(i64),
}

/// A value read for a comparison with the outer row's or another column (`How`'s: a
/// timestamp of `How::Instant` an instant; a stored string in place).
#[derive(Clone, Copy)]
enum Got<'a> {
    Num((i128, u32)),
    Text(&'a [u8]),
    Date(i32),
    Ts(i64),
}

impl How {
    /// Value `v` as this comparison reads it (Some(None): NULL; None: a value it doesn't take).
    fn of_value(self, v: &Value) -> Option<Option<Got<'_>>> {
        if v.is_null() {
            return Some(None);
        }
        Some(Some(match (self, v) {
            (How::Num, v) => Got::Num(small_num(v)?),
            (How::Str(true), Value::Text(s)) => Got::Text(crate::types::text::bpchar_trim(s).as_bytes()),
            (How::Str(false), Value::Text(s)) => Got::Text(s.as_bytes()),
            (How::Str(false), Value::Date(d)) => Got::Date(*d),
            (How::Str(false), Value::Timestamp(t)) => Got::Ts(*t),
            (How::Instant, Value::Date(d)) => Got::Ts(crate::sql::func::ops::date_instant(*d)),
            (How::Instant, Value::Timestamp(t)) => Got::Ts(*t),
            _ => return None,
        }))
    }

    /// The stored value at `pos` of `data` as this comparison reads it (as `of_value`).
    #[inline]
    fn peek(self, data: &[u8], pos: usize) -> Option<Option<Got<'_>>> {
        use crate::storage::row::{Dt, peek_dt, peek_num, peek_text};
        Some(Some(match self {
            How::Num => Got::Num(peek_num(data, pos)??),
            How::Str(bp) if data.get(pos) == Some(&crate::storage::row::tag::TEXT) || bp => {
                let x = peek_text(data, pos)??;
                Got::Text(if bp { crate::storage::cols::trim_spaces(x) } else { x })
            }
            How::Str(_) => match peek_dt(data, pos)?? {
                Dt::Date(d) => Got::Date(d),
                Dt::Ts(t) => Got::Ts(t),
            },
            How::Instant => match peek_dt(data, pos)?? {
                Dt::Date(d) => Got::Ts(crate::sql::func::ops::date_instant(d)),
                Dt::Ts(t) => Got::Ts(t),
            },
        }))
    }
}

/// `x` against `y` (None: values of kinds that don't compare this way, or numbers that don't
/// scale to one scale in an i128).
#[inline]
fn cmp_got(x: Got<'_>, y: Got<'_>) -> Option<std::cmp::Ordering> {
    Some(match (x, y) {
        (Got::Num((x, sx)), Got::Num((y, sy))) => {
            if sx == sy {
                x.cmp(&y)
            } else {
                let s = sx.max(sy);
                let pow = |n: u32| 10i128.checked_pow(n);
                x.checked_mul(pow(s - sx)?)?.cmp(&y.checked_mul(pow(s - sy)?)?)
            }
        }
        (Got::Text(x), Got::Text(y)) => x.cmp(y),
        (Got::Date(x), Got::Date(y)) => x.cmp(&y),
        (Got::Ts(x), Got::Ts(y)) => x.cmp(&y),
        _ => return None,
    })
}

/// Scans that made a `StoredCmps` (tests: which filters run over stored bytes).
pub static STORED_CMPS_SCANS: crate::counter::Counter = crate::counter::Counter::new();

impl StoredCmps {
    /// Over a scan's own row: column `i` is the table's.
    pub fn of(k: &K) -> Option<StoredCmps> {
        let ks: Vec<&K> = match k {
            K::And(ks) => ks.iter().collect(),
            k => vec![k],
        };
        let (c, _) = Self::make(&ks, None).filter(|(_, n)| *n == ks.len())?;
        STORED_CMPS_SCANS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(c)
    }

    /// Over a nested loop's joined row (outer row `0..nleft`, the inner scan's row after it),
    /// the first of conjuncts `ks` that run over the inner row's stored values, and how many:
    /// an outer column's value is taken from the outer row `passes_with` is given.
    pub fn of_joined(ks: &[&K], nleft: usize) -> Option<(StoredCmps, usize)> {
        let r = Self::make(ks, Some(nleft))?;
        STORED_CMPS_SCANS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(r)
    }

    fn make(ks: &[&K], nleft: Option<usize>) -> Option<(StoredCmps, usize)> {
        // (A column of the stored row; None: of the outer row, or not one.)
        let inner = |i: usize| match nleft {
            None => Some(i),
            Some(n) => i.checked_sub(n),
        };
        let outer = |i: usize| nleft.filter(|n| i < *n).map(|_| i);
        // (A column, or an integer operator over a column and a constant.)
        let col = |k: &K| match k {
            K::Col(i) => Some((inner(*i)?, None)),
            K::Int { op, args, ret, .. } => match &**args {
                [K::Col(i), K::Const(c)] => Some((inner(*i)?, Some((*op, small_num(c)?, true, ret.clone())))),
                [K::Const(c), K::Col(i)] => Some((inner(*i)?, Some((*op, small_num(c)?, false, ret.clone())))),
                _ => None,
            },
            _ => None,
        };
        // (The other operand: a constant as `how` reads it, the outer row's column, or a
        // plain column of the stored row.)
        let other = |k: &K, how: How| -> Option<Other> {
            match k {
                K::Const(v) => Some(Other::Const(match (how, v) {
                    (How::Num, v) => Against::Num(small_num(v)?),
                    (How::Str(true), Value::Text(s)) => Against::Text(crate::types::text::bpchar_trim(s).as_bytes().into(), true),
                    (How::Str(false), Value::Text(s)) => Against::Text(s.as_bytes().into(), false),
                    (How::Str(false), Value::Date(d)) => Against::Date(*d),
                    (How::Str(false), Value::Timestamp(t)) => Against::Ts(*t),
                    (How::Instant, Value::Date(d)) => Against::Instant(crate::sql::func::ops::date_instant(*d)),
                    (How::Instant, Value::Timestamp(t)) => Against::Instant(*t),
                    _ => return None,
                })),
                K::Col(i) => match outer(*i) {
                    Some(j) => Some(Other::Outer(j)),
                    None => Some(Other::Col(inner(*i)?)),
                },
                _ => None,
            }
        };
        let one = |k: &K| -> Option<StoredCmp> {
            let (how, test, args) = match k {
                K::NumCmp { test, args, .. } => (How::Num, *test, &**args),
                K::Call { f, args, .. } => {
                    use crate::sql::func::ops;
                    match ops::str_cmp(*f) {
                        Some((test, bp)) => (How::Str(bp), test, &**args),
                        None => (How::Instant, ops::dt_cmp(*f)?, &**args),
                    }
                }
                _ => return None,
            };
            // (The stored column first where both are; an integer operator only over a
            // stored column, and not of a comparison of two of them.)
            let [a, b] = args;
            let (c, o, col_first) = match (col(a), col(b)) {
                (Some(c), _) => (c, b, true),
                (None, Some(c)) => (c, a, false),
                (None, None) => return None,
            };
            let ((col, op), other) = (c, other(o, how)?);
            if op.is_some() && !matches!(how, How::Num) || op.is_some() && matches!(other, Other::Col(_)) {
                return None;
            }
            Some(StoredCmp { col, op, how, other, test, col_first })
        };
        let mut cmps: Vec<StoredCmp> = ks.iter().map_while(|k| one(k)).collect();
        let used = cmps.len();
        // (Pure comparisons: their order changes nothing.)
        cmps.sort_by_key(|c| c.col);
        let upto = cmps
            .iter()
            .map(|c| match c.other {
                Other::Col(o) => c.col.max(o) + 1,
                _ => c.col + 1,
            })
            .max()?;
        // (A comparison of two of the row's columns reads them where the walk found them.)
        if upto > MAX_STORED_COLS && cmps.iter().any(|c| matches!(c.other, Other::Col(_))) {
            return None;
        }
        let pairs = cmps.iter().any(|c| matches!(c.other, Other::Col(_)));
        let exact = cmps.iter().all(|c| c.op.is_none());
        Some((StoredCmps { cmps, upto, pairs, exact }, used))
    }

    /// The stored row's columns it reads, marked in `cols` (false: one past its end).
    pub fn mark_cols(&self, cols: &mut [bool]) -> bool {
        let mut ok = true;
        for c in &self.cmps {
            for x in [Some(c.col), if let Other::Col(x) = c.other { Some(x) } else { None }].into_iter().flatten() {
                match cols.get_mut(x) {
                    Some(b) => *b = true,
                    None => ok = false,
                }
            }
        }
        ok
    }

    /// Whether the stored row's values `data` (after its header) pass (None: leave it to
    /// the kernel).
    pub fn passes(&self, data: &[u8]) -> Option<bool> {
        self.passes_with(data, &[])
    }

    /// `passes` with outer row `outer` (`of_joined`'s).
    pub fn passes_with(&self, data: &[u8], outer: &[Value]) -> Option<bool> {
        use crate::storage::{cols::skip, row::{Dt, peek_dt, peek_num, peek_text}, sst::get_varint};
        let mut pos = 0;
        let n = get_varint(data, &mut pos).ok()? as usize;
        if n < self.upto {
            return None;
        }
        // (The comparisons in column order: the columns walked once, each one's place kept
        // for a comparison with another column.)
        let mut at = [0u32; MAX_STORED_COLS];
        let (mut col, mut pass) = (0, true);
        for c in &self.cmps {
            // (Without errors to raise, a comparison not true ends it: the AND isn't.)
            if !pass && self.exact {
                return Some(false);
            }
            while col < c.col {
                if self.pairs {
                    at[col] = pos as u32;
                }
                skip(data, &mut pos).ok()?;
                col += 1;
            }
            if self.pairs {
                at[col] = pos as u32;
            }
            // (NULL: the comparison is NULL, and the AND not true; the others still read, as
            // a value one of them can't decide leaves the row to the kernel.)
            if let Other::Const(against) = &c.other {
                // (A constant: its kind known, the stored value read as that.)
                let o = match against {
                    Against::Num((y, sy)) => {
                        let Some(mut v) = peek_num(data, pos)? else {
                            pass = false;
                            continue;
                        };
                        if let Some((op, k, first, ret)) = &c.op {
                            let (a, b) = if *first { (v, *k) } else { (*k, v) };
                            v = (op.apply(a, b, ret)?, 0);
                        }
                        let (x, sx) = v;
                        if sx == *sy {
                            x.cmp(y)
                        } else {
                            let s = sx.max(*sy);
                            let pow = |n: u32| 10i128.checked_pow(n);
                            x.checked_mul(pow(s - sx)?)?.cmp(&y.checked_mul(pow(s - sy)?)?)
                        }
                    }
                    Against::Text(y, bp) => {
                        let Some(mut x) = peek_text(data, pos)? else {
                            pass = false;
                            continue;
                        };
                        if *bp {
                            x = crate::storage::cols::trim_spaces(x);
                        }
                        x.cmp(y)
                    }
                    Against::Date(_) | Against::Ts(_) | Against::Instant(_) => {
                        let Some(x) = peek_dt(data, pos)? else {
                            pass = false;
                            continue;
                        };
                        match (against, x) {
                            (Against::Date(y), Dt::Date(x)) => x.cmp(y),
                            (Against::Ts(y), Dt::Ts(x)) => x.cmp(y),
                            (Against::Instant(y), Dt::Date(x)) => crate::sql::func::ops::date_instant(x).cmp(y),
                            (Against::Instant(y), Dt::Ts(x)) => x.cmp(y),
                            _ => return None,
                        }
                    }
                };
                pass &= (c.test)(if c.col_first { o } else { o.reverse() });
                continue;
            }
            let Some(mut x) = c.how.peek(data, pos)? else {
                pass = false;
                continue;
            };
            if let (Some((op, k, first, ret)), Got::Num(v)) = (&c.op, x) {
                let (a, b) = if *first { (v, *k) } else { (*k, v) };
                x = Got::Num((op.apply(a, b, ret)?, 0));
            }
            let y = match &c.other {
                Other::Const(_) => return None,
                Other::Outer(j) => match c.how.of_value(outer.get(*j)?)? {
                    Some(y) => y,
                    None => {
                        pass = false;
                        continue;
                    }
                },
                Other::Col(o) => {
                    // (A column before this one: where the walk found it; after: walked to.)
                    let p = if *o <= c.col {
                        at[*o] as usize
                    } else {
                        let mut q = pos;
                        let mut k = c.col;
                        while k < *o {
                            skip(data, &mut q).ok()?;
                            k += 1;
                        }
                        q
                    };
                    match c.how.peek(data, p)? {
                        Some(y) => y,
                        None => {
                            pass = false;
                            continue;
                        }
                    }
                }
            };
            let o = cmp_got(x, y)?;
            pass &= (c.test)(if c.col_first { o } else { o.reverse() });
        }
        Some(pass)
    }
}

/// The columns a comparison of two of a stored row's columns may reach (`StoredCmps`).
const MAX_STORED_COLS: usize = 32;

/// How deep a kernel may nest: compiling and running one recurse as deep as it goes, with no
/// stack check, so a deeper expression (an UPDATE through a view whose WHERE is 2,000 terms
/// deep overflowed a 2 MB thread here) is evaluated as an expression, whose recursion is
/// guarded.
const MAX_KERNEL_DEPTH: u32 = 256;

thread_local! {
    static COMPILE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// `e` as a kernel (None: it holds something kernels don't do, or nests too deep).
pub fn compile(e: &Expr) -> Option<K> {
    struct Level;
    impl Drop for Level {
        fn drop(&mut self) {
            COMPILE_DEPTH.with(|d| d.set(d.get() - 1));
        }
    }
    if COMPILE_DEPTH.with(|d| {
        d.set(d.get() + 1);
        d.get()
    }) > MAX_KERNEL_DEPTH
    {
        COMPILE_DEPTH.with(|d| d.set(d.get() - 1));
        return None;
    }
    let _level = Level;
    compile_node(e)
}

/// `compile` of `e` with the statement's parameters (`$n`) taken as the constants they are
/// for this execution (`ExecCtx::param`, which `eval` reads them from): a prepared
/// statement's generic plan (`WHERE bid = $1`) compiles as its custom plan does. Only for a
/// kernel made and dropped within one execution (a node's, made again after `park`).
pub fn compile_bound(e: &Expr, ctx: &super::ExecCtx) -> Option<K> {
    let mut params = false;
    e.walk(&mut |x| params |= matches!(x, Expr::Param(..)));
    if !params {
        return compile(e);
    }
    let bound = e
        .clone()
        .transform(&mut |x| match x {
            Expr::Param(i, t) => Ok(Expr::Const(ctx.param(i)?, t)),
            x => Ok(x),
        })
        .ok()?;
    compile(&bound)
}

fn compile_node(e: &Expr) -> Option<K> {
    Some(match e {
        Expr::Col(i, _) => K::Col(*i),
        Expr::Const(v, _) => K::Const(v.clone()),
        Expr::Relabel(e, _) | Expr::SqlSyntax(e) | Expr::Collate(e, ..) => compile(e)?,
        // (A cast that leaves a string as it is, to an unbounded type of its own kind or
        // varchar to text, is no cast: `l_shipmode::bpchar` of a char(10). bpchar to text
        // trims: not one.)
        Expr::Cast(e, from, to, _)
            if matches!(
                (from, to),
                (DataType::Bpchar(_), DataType::Bpchar(-1))
                    | (DataType::Varchar(_), DataType::Varchar(-1) | DataType::Text)
                    | (DataType::Text, DataType::Text | DataType::Varchar(-1))
            ) =>
        {
            compile(e)?
        }
        Expr::Cast(e, from, to, explicit) if cast_pure(from, to) => {
            K::Cast(Box::new(compile(e)?), Box::new((from.clone(), to.clone())), *explicit)
        }
        Expr::And(args) => K::And(args.iter().map(compile).collect::<Option<_>>()?),
        Expr::Or(args) => K::Or(args.iter().map(compile).collect::<Option<_>>()?),
        Expr::Not(a) => K::Not(Box::new(compile(a)?)),
        Expr::IsNull(a, neg) => K::IsNull(Box::new(compile(a)?), *neg),
        Expr::Call(def, args, DataType::Float8) if args.len() == 2 && def.strict && !def.spread && let Some(f) = crate::ext::vector::distance_kernel(def.imp) => {
            K::Dist(f, Box::new([compile(&args[0])?, compile(&args[1])?]))
        }
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
        Expr::Call(def, args, ty)
            if args.len() == 2
                && def.strict
                && matches!(ty, DataType::Int2 | DataType::Int4 | DataType::Int8)
                && args.iter().all(|a| matches!(a.ty(), DataType::Int2 | DataType::Int4 | DataType::Int8))
                && let Some(op) = crate::sql::func::ops::int_op(def.imp) =>
        {
            K::Int {
                op,
                f: pure_call(def, ty)?,
                args: Box::new([compile(&args[0])?, compile(&args[1])?]),
                ret: ty.clone(),
            }
        }
        // (Integers too: a column read in place compares without being made a value.)
        Expr::Call(def, args, ty)
            if args.len() == 2
                && def.strict
                && args.iter().all(|a| {
                    matches!(a.ty(), DataType::Numeric(_) | DataType::Int2 | DataType::Int4 | DataType::Int8)
                })
                && let Some(test) = crate::sql::func::ops::cmp_op(def.imp) =>
        {
            K::NumCmp {
                test,
                f: pure_call(def, ty)?,
                args: Box::new([compile(&args[0])?, compile(&args[1])?]),
            }
        }
        Expr::Call(def, args, DataType::Bool)
            if args.len() == 2
                && def.strict
                && !def.spread
                && def.coll.as_ref().is_none_or(|c| c.deterministic)
                && matches!(args[0].ty(), DataType::Text | DataType::Varchar(_))
                && let Some(neg) = crate::sql::func::string::like_kind(def.imp)
                && let Expr::Const(Value::Text(p), _) = &args[1] =>
        {
            let pat = crate::sql::func::string::LikePattern::new(p, Some('\\')).ok()?;
            K::Like(Box::new(compile(&args[0])?), Box::new(pat), neg)
        }
        Expr::Call(def, args, DataType::Text)
            if args.len() == 3
                && def.strict
                && crate::sql::func::string::is_substr(def.imp)
                && let (Expr::Const(Value::Int4(start), _), Expr::Const(Value::Int4(count), _)) = (&args[1], &args[2])
                && *count >= 0 =>
        {
            K::Substr(Box::new(compile(&args[0])?), *start as i64, *count as i64)
        }
        // (EXTRACT of a date's year, month or day: the unit read once, here.)
        Expr::Call(def, args, ty)
            if args.len() == 2
                && pure_call(def, ty).is_some()
                && let Expr::Const(Value::Text(u), _) = &args[0]
                && let Some(f) = crate::sql::func::datetime::extract_date_field(def.imp, u) =>
        {
            K::Call { f, args: Box::new([compile(&args[0])?, compile(&args[1])?]), strict: def.strict, ret: ty.clone() }
        }
        Expr::Call(def, args, ty) if args.len() == 2 => K::Call {
            f: pure_call(def, ty).or_else(|| collated_bytes(def, args))?,
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
        Expr::InList(x, list, neg) => match int_in(x, list, *neg) {
            Some(k) => k,
            None => K::InList(
                Box::new(compile(x)?),
                list.iter()
                    .map(|(item, eq)| Some((compile(item)?, pure_call(eq, &DataType::Bool)?)))
                    .collect::<Option<_>>()?,
                *neg,
            ),
        },
        Expr::ArrayOp { left, right, op, all } => K::ArrayOp(
            Box::new([compile(left)?, compile(right)?]),
            pure_call(op, &DataType::Bool)?,
            *all,
        ),
        _ => return None,
    })
}

/// `x [NOT] IN (…)` as `K::IntIn`: an integer `x`, every item an integer constant (or NULL)
/// compared by the built-in `=` of two integer types.
fn int_in(x: &Expr, list: &[(Expr, std::sync::Arc<crate::sql::func::FuncDef>)], neg: bool) -> Option<K> {
    let int = |t: &DataType| matches!(t, DataType::Int2 | DataType::Int4 | DataType::Int8);
    if list.is_empty() || !int(&x.ty()) {
        return None;
    }
    let mut vals = Vec::with_capacity(list.len());
    let mut null = false;
    for (item, eq) in list {
        if eq.name != "=" || eq.oid >= crate::catalog::FIRST_USER_OID || eq.args.len() != 2 || !eq.args.iter().all(int) {
            return None;
        }
        match item {
            Expr::Const(Value::Int2(v), _) => vals.push(*v as i64),
            Expr::Const(Value::Int4(v), _) => vals.push(*v as i64),
            Expr::Const(Value::Int8(v), _) => vals.push(*v),
            Expr::Const(Value::Null, _) => null = true,
            _ => return None,
        }
    }
    vals.sort_unstable();
    vals.dedup();
    Some(K::IntIn(Box::new(compile(x)?), vals.into(), null, neg))
}

impl K {
    /// A distance call over a column and a constant of a type whose kernel reads bytes (the
    /// column, the constant's bytes, whether the column is the first argument).
    pub fn dist_col(&self) -> Option<(usize, &[u8], bool)> {
        let K::Dist(f, args) = self else { return None };
        let (kind, _) = f.bytes?;
        match &**args {
            [K::Col(c), K::Const(Value::Ext(k, q))] if *k == kind => Some((*c, &q[..], true)),
            [K::Const(Value::Ext(k, q)), K::Col(c)] if *k == kind => Some((*c, &q[..], false)),
            _ => None,
        }
    }

    /// `dist_col`'s value over the encoded row `data` (its column not decoded): None where
    /// the stored value isn't one of the type or the dimensions differ (leave it to `eval`).
    pub fn dist_raw(&self, data: &[u8]) -> Option<Value> {
        let (c, ..) = self.dist_col()?;
        let (k, x) = crate::storage::row::ext_at(data, c)?;
        self.dist_ext(k, x)
    }

    /// `dist_raw` over the column's value given apart (kind and bytes: one stored out of
    /// line).
    pub fn dist_ext(&self, k: u16, x: &[u8]) -> Option<Value> {
        let (_, q, first) = self.dist_col()?;
        let K::Dist(f, _) = self else { return None };
        let (kind, bf) = f.bytes?;
        if k != kind || !crate::ext::vector::kernel_bytes_agree(kind, x, q) {
            return None;
        }
        Some(Value::Float8(if first { bf(x, q) } else { bf(q, x) }))
    }

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
            K::Col(i) => row.num(*i),
            K::Const(v) => leaf(v),
            K::Int { op, args, ret, .. } => {
                // (Both operands before a NULL one: the call is strict.)
                let (a, b) = (args[0].num(row)?, args[1].num(row)?);
                let (Some(x), Some(y)) = (a, b) else {
                    return Some(None);
                };
                Some(Some((op.apply(x, y, ret)?, 0)))
            }
            K::Num { op, args, .. } => {
                // (Both operands before a NULL one: the call is strict.)
                let (a, b) = (args[0].num(row)?, args[1].num(row)?);
                let (Some((x, sx)), Some((y, sy))) = (a, b) else {
                    return Some(None);
                };
                Some(Some(num_op(*op, x, sx, y, sy)?))
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
                            crate::types::decint::DecInt::from(v),
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
            K::Int { f, args, ret, .. } => {
                if let Some(r) = self.num(row) {
                    return Some(r.map_or(Value::Null, |(v, _)| int_value(v, ret)));
                }
                let (mut t0, mut t1) = (Value::Null, Value::Null);
                let x = args[0].arg(row, &mut t0)?;
                let y = args[1].arg(row, &mut t1)?;
                if x.is_null() || y.is_null() {
                    return Some(Value::Null);
                }
                f(x, y, ret).ok()?
            }
            K::Col(i) => row.col(*i)?.clone(),
            K::Const(v) => v.clone(),
            K::NumCmp { test, f, args } => {
                if let (Some(a), Some(b)) = (args[0].num(row), args[1].num(row)) {
                    let (Some((x, sx)), Some((y, sy))) = (a, b) else {
                        return Some(Value::Null);
                    };
                    let s = sx.max(sy);
                    let pow = |n: u32| 10i128.checked_pow(n);
                    let scaled = (|| Some((x.checked_mul(pow(s - sx)?)?, y.checked_mul(pow(s - sy)?)?)))();
                    if let Some((x, y)) = scaled {
                        return Some(Value::Bool(test(x.cmp(&y))));
                    }
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
                f(x, y, &DataType::Bool).ok()?
            }
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
            K::Dist(f, args) => {
                let (mut t0, mut t1) = (Value::Null, Value::Null);
                let x = args[0].arg(row, &mut t0)?;
                let y = args[1].arg(row, &mut t1)?;
                if x.is_null() || y.is_null() {
                    return Some(Value::Null);
                }
                // (Dimensions that differ: the function's error, which `eval` reports.)
                if !crate::ext::vector::kernel_dims_agree(x, y) {
                    return None;
                }
                Value::Float8((f.of)(x, y))
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
            K::IntIn(x, vals, null, neg) => {
                let mut t = Value::Null;
                let v = match x.arg(row, &mut t)? {
                    Value::Int2(v) => *v as i64,
                    Value::Int4(v) => *v as i64,
                    Value::Int8(v) => *v,
                    Value::Null => return Some(Value::Null),
                    _ => return None,
                };
                // (A few items: compared in turn; more: by halves.)
                let hit = if vals.len() <= 8 { vals.contains(&v) } else { vals.binary_search(&v).is_ok() };
                match (hit, *null) {
                    (true, _) => Value::Bool(!*neg),
                    (false, true) => Value::Null,
                    (false, false) => Value::Bool(*neg),
                }
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
            // (A column's string read where it is: a row read in place makes no value.)
            K::Like(a, pat, neg) if matches!(**a, K::Col(_)) => {
                let K::Col(i) = **a else { unreachable!("a column") };
                match row.text(i)? {
                    None => Value::Null,
                    Some(s) => Value::Bool(pat.matches(s).ok()? != *neg),
                }
            }
            K::Like(a, pat, neg) => {
                let mut t = Value::Null;
                match a.arg(row, &mut t)? {
                    Value::Null => Value::Null,
                    Value::Text(s) => Value::Bool(pat.matches(s).ok()? != *neg),
                    _ => return None,
                }
            }
            K::Substr(a, start, count) => {
                let mut t = Value::Null;
                match a.arg(row, &mut t)? {
                    Value::Null => Value::Null,
                    Value::Text(s) => Value::text(crate::sql::func::string::substr_chars(s, *start, Some(*count)).ok()?),
                    _ => return None,
                }
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

    /// The row's value as a condition (Some(None): NULL; None: leave it to `eval`).
    pub fn truth<R: Cols + ?Sized>(&self, row: &R) -> Option<Option<bool>> {
        Some(truthy(&self.run(row)?))
    }

    /// Whether the row passes as a filter (None: leave it to `eval`).
    pub fn passes<R: Cols + ?Sized>(&self, row: &R) -> Option<bool> {
        Some(truthy(&self.run(row)?) == Some(true))
    }
}

#[cfg(test)]
mod tests {
    use super::{NumOp, num_op};

    /// `num_op`'s fast paths give what the checked i128 arithmetic gives, overflows (None)
    /// included, around the i64 and i128 limits and numeric's scale limit.
    #[test]
    fn num_op_is_the_checked_arithmetic() {
        let reference = |op: NumOp, x: i128, sx: u32, y: i128, sy: u32| -> Option<(i128, u32)> {
            let pow = |n: u32| 10i128.checked_pow(n);
            Some(match op {
                NumOp::Add | NumOp::Sub => {
                    let s = sx.max(sy);
                    let (x, y) = (x.checked_mul(pow(s - sx)?)?, y.checked_mul(pow(s - sy)?)?);
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
        };
        let big = [0, 1, -1, 7, 10_000, i64::MAX as i128, i64::MIN as i128, i64::MAX as i128 + 1, i64::MIN as i128 - 1, 1 << 100, -(1 << 100), i128::MAX, i128::MIN];
        let scales = [0, 2, 4, 18, 19, 38, 39, 40, 16383];
        for op in [NumOp::Add, NumOp::Sub, NumOp::Mul] {
            for &x in &big {
                for &y in &big {
                    for &sx in &scales {
                        for &sy in &scales[..4] {
                            assert_eq!(num_op(op, x, sx, y, sy), reference(op, x, sx, y, sy), "{op:?} {x} {sx} {y} {sy}");
                            assert_eq!(num_op(op, y, sy, x, sx), reference(op, y, sy, x, sx), "{op:?} {y} {sy} {x} {sx}");
                        }
                    }
                }
            }
        }
    }
}
