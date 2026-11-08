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
pub(super) fn pure_call(def: &crate::sql::func::FuncDef, ty: &DataType) -> Option<PureFn> {
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

/// A filter of comparisons of columns with constants, one or ANDed, run over a stored row's
/// encoded values in place: the columns walked once, nothing decoded, made or cached.
/// Numeric columns (`K::NumCmp`) compare as exact numbers (`row::peek_num`), string ones
/// (`K::Call` of a string comparison that compares bytes: `ops::str_cmp`, `collated_bytes`)
/// as their stored bytes (`row::peek_text`), bpchar's without trailing spaces. What a row's
/// values don't decide this way (a column added since it was stored, a number past an
/// i128, a value stored out of line) is left to the kernel.
pub struct StoredCmps {
    /// (column, the constant, the operator's test, whether the column is its first
    /// argument).
    cmps: Vec<(usize, Against, fn(std::cmp::Ordering) -> bool, bool)>,
    /// The columns walked: up to the last one compared.
    upto: usize,
}

/// The constant a stored value is compared with.
enum Against {
    Num((i128, u32)),
    /// A string's bytes; bpchar's (trailing spaces not compared) when true, the constant's
    /// trimmed already.
    Text(Box<[u8]>, bool),
}

/// Scans that made a `StoredCmps` (tests: which filters run over stored bytes).
pub static STORED_CMPS_SCANS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl StoredCmps {
    pub fn of(k: &K) -> Option<StoredCmps> {
        let one = |k: &K| match k {
            K::NumCmp { test, args, .. } => match &**args {
                [K::Col(i), K::Const(c)] => Some((*i, Against::Num(small_num(c)?), *test, true)),
                [K::Const(c), K::Col(i)] => Some((*i, Against::Num(small_num(c)?), *test, false)),
                _ => None,
            },
            K::Call { f, args, .. } => {
                let (test, bp) = crate::sql::func::ops::str_cmp(*f)?;
                let text = |c: &Value| match c {
                    Value::Text(s) if bp => Some(Against::Text(crate::types::text::bpchar_trim(s).as_bytes().into(), true)),
                    Value::Text(s) => Some(Against::Text(s.as_bytes().into(), false)),
                    _ => None,
                };
                match &**args {
                    [K::Col(i), K::Const(c)] => Some((*i, text(c)?, test, true)),
                    [K::Const(c), K::Col(i)] => Some((*i, text(c)?, test, false)),
                    _ => None,
                }
            }
            _ => None,
        };
        let mut cmps: Vec<_> = match k {
            K::And(ks) => ks.iter().map(one).collect::<Option<_>>()?,
            k => vec![one(k)?],
        };
        // (Pure comparisons: their order changes nothing.)
        cmps.sort_by_key(|c| c.0);
        let upto = cmps.iter().map(|c| c.0 + 1).max()?;
        STORED_CMPS_SCANS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(StoredCmps { cmps, upto })
    }

    /// Whether the stored row's values `data` (after its header) pass (None: leave it to
    /// the kernel).
    pub fn passes(&self, data: &[u8]) -> Option<bool> {
        use crate::storage::{row::{peek_num, peek_text, skip_value_fast}, sst::get_varint};
        let mut pos = 0;
        let n = get_varint(data, &mut pos).ok()? as usize;
        if n < self.upto {
            return None;
        }
        // (The comparisons in column order: the columns walked once.)
        let (mut col, mut pass) = (0, true);
        for (i, against, test, col_first) in &self.cmps {
            while col < *i {
                skip_value_fast(data, &mut pos).ok()?;
                col += 1;
            }
            // (NULL: the comparison is NULL, and the AND not true; the others still read, as
            // a value one of them can't decide leaves the row to the kernel.)
            let o = match against {
                Against::Num((y, sy)) => {
                    let Some((x, sx)) = peek_num(data, pos)? else {
                        pass = false;
                        continue;
                    };
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
                        while let [rest @ .., b' '] = x {
                            x = rest;
                        }
                    }
                    x.cmp(y)
                }
            };
            pass &= test(if *col_first { o } else { o.reverse() });
        }
        Some(pass)
    }
}

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
            K::Like(a, pat, neg) => {
                let mut t = Value::Null;
                match a.arg(row, &mut t)? {
                    Value::Null => Value::Null,
                    Value::Text(s) => Value::Bool(pat.matches(s).ok()? != *neg),
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
