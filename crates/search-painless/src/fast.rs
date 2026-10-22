//! A fast path for the expression scripts scoring, sorting and runtime fields run per document
//! (`doc['likes'].value * params.factor`, `Math.log(2 + doc['pop'].value) * _score`,
//! `decayNumericGauss(...)`, `emit(doc['price'].value * 1.1)`).
//!
//! Such a script is straight-line code — local declarations, `if`/`else`, `return` — over
//! expressions of constants, locals, `_score`, `params.x`, `doc['f'].value`, `doc['f'].size()`,
//! `doc['f'].empty`/`isEmpty()`, arithmetic, comparisons, conditionals, casts, `Math` functions and the
//! score functions (no loops, no stores but declarations, no other calls). At compile time the typed tree is reduced to a [`Plan`] (or rejected). The
//! first execution of an [`crate::Instance`] specializes the plan on what it then knows: the
//! values of `params` (fixed for the instance) and the doc-values kind of each field (fixed by
//! the mapping) decide every `def` operation's type, so each node becomes a closure over unboxed
//! `int`/`long`/`float`/`double`/`boolean` values — no instruction dispatch, no tagged values,
//! no frame set-up.
//!
//! The closures implement exactly the semantics the interpreter would (Java's conversions,
//! `DefMath`'s promotions, IEEE arithmetic); anything that would throw or that the
//! specialization did not foresee — an empty field, a field of another kind, an integer
//! division by zero, a failing library call — bails out, and the document is run through the
//! interpreter, which produces the result or the exact error.

use crate::ast::Op;
use crate::contexts::ContextDef;
use crate::ir::{Cast, Const, DefOp, Place, Prim, TExpr, TStmt, TK};
use crate::lookup::{registry, MethodId};
use crate::types::{cls, Ty};
use crate::value::Value;
use crate::vm::Vm;

/// A script reduced to fast-path statements: declarations of locals, `if`/`else` and
/// `return`s of expressions.
#[derive(Debug)]
pub struct Plan {
    body: Vec<FStmt>,
}

#[derive(Debug, Clone)]
enum FStmt {
    /// A local's declaration; its type is the initializer's specialized type.
    Decl { slot: u32, init: FExpr },
    If { c: FExpr, then: Vec<FStmt>, els: Vec<FStmt> },
    Return(FExpr),
    /// `emit(x)` of a `long`, `double`, `date` or `boolean` runtime field.
    Emit { kind: EmitTo, arg: FExpr },
    /// `return;` of a runtime field.
    Done,
    /// `x = e` / `x op= e` / `x++` of a primitive local; `value` is the new value in the
    /// local's type.
    Assign { slot: u32, value: FExpr },
    /// `for (x : doc['f'])` over numeric or boolean doc values; `conv` is the loop variable's
    /// primitive type (`None` for `def`, which takes the values as they are).
    EachDoc { field: Box<str>, slot: u32, conv: Option<Prim>, body: Vec<FStmt> },
}

/// The value a runtime field's `emit` collects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitTo {
    Long,
    Double,
    Bool,
}

#[derive(Debug, Clone)]
enum FExpr {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Bool(bool),
    Score,
    Param(Box<str>),
    Doc(Box<str>),
    /// `doc['f'].size()` / `doc['f'].empty`.
    DocSize(Box<str>),
    DocEmpty(Box<str>),
    Local(u32),
    Cast(Box<FExpr>, Cast),
    Bin { op: Op, l: Box<FExpr>, r: Box<FExpr>, operand: Option<Prim>, def: Option<DefOp> },
    Neg { x: Box<FExpr>, operand: Option<Prim>, def: Option<DefOp> },
    Cmp { op: Op, l: Box<FExpr>, r: Box<FExpr>, operand: Option<Prim> },
    Not(Box<FExpr>),
    And(Box<FExpr>, Box<FExpr>),
    Or(Box<FExpr>, Box<FExpr>),
    Cond(Box<FExpr>, Box<FExpr>, Box<FExpr>),
    /// A pure static method or score function: `ctx` calls set the binding slot first.
    Call { m: MethodId, args: Vec<FExpr>, binding: Option<u32>, ctx: bool },
}

/// Static `Math` methods over primitives the fast path calls (all pure).
const MATH: &[&str] = &[
    "abs", "acos", "asin", "atan", "atan2", "cbrt", "ceil", "cos", "cosh", "exp", "expm1", "floor", "hypot", "log", "log10", "log1p", "max", "min", "pow", "rint",
    "signum", "sin", "sinh", "sqrt", "tan", "tanh", "toDegrees", "toRadians",
];
/// Score functions with no per-document host access beyond what they read themselves.
const SCORE_FNS: &[&str] = &["saturation", "sigmoid", "decayNumericLinear", "decayNumericExp", "decayNumericGauss"];

/// Reduces `execute` to a plan when the script has the supported shape.
pub fn plan(exec: &crate::ir::TFunction, def: &ContextDef) -> Option<Plan> {
    let void = def.ret == Ty::VOID && def.emit_kind().is_some();
    // `def` results (aggregation scripts, script fields) are returned boxed
    let boxed = def.ret == Ty::DEF && def.var_index("doc").is_some();
    if !matches!(def.ret, Ty::DOUBLE | Ty::BOOLEAN) && !void && !boxed {
        return None;
    }
    let vars = Vars {
        score: def.var_index("_score").map(|i| i as u32),
        params: def.var_index("params").map(|i| i as u32),
        doc: def.var_index("doc").map(|i| i as u32),
        first_local: def.vars.len() as u32,
        locals: exec.local_tys.clone(),
    };
    let body = stmts(&exec.body, &vars)?;
    if void {
        // a runtime field: emits, branches and loops, no values returned
        if !emits_only(&body) {
            return None;
        }
        return Some(Plan { body });
    }
    // the last statement must leave the function
    if !returns(&body) || has_emit(&body) || has_done(&body) {
        return None;
    }
    Some(Plan { body })
}

fn emits_only(b: &[FStmt]) -> bool {
    b.iter().all(|s| match s {
        FStmt::Return(_) => false,
        FStmt::Done => true,
        FStmt::If { then, els, .. } => emits_only(then) && emits_only(els),
        FStmt::EachDoc { body, .. } => emits_only(body),
        _ => true,
    })
}

fn count_emits(b: &[FStmt]) -> usize {
    b.iter()
        .map(|s| match s {
            FStmt::Emit { .. } => 1,
            FStmt::If { then, els, .. } => count_emits(then) + count_emits(els),
            FStmt::EachDoc { body, .. } => count_emits(body),
            _ => 0,
        })
        .fold(0, usize::saturating_add)
}

fn has_emit(b: &[FStmt]) -> bool {
    count_emits(b) > 0
}

fn has_done(b: &[FStmt]) -> bool {
    b.iter().any(|s| match s {
        FStmt::Done => true,
        FStmt::If { then, els, .. } => has_done(then) || has_done(els),
        FStmt::EachDoc { body, .. } => has_done(body),
        _ => false,
    })
}

fn returns(b: &[FStmt]) -> bool {
    match b.last() {
        Some(FStmt::Return(_)) => true,
        Some(FStmt::If { then, els, .. }) => returns(then) && returns(els),
        _ => false,
    }
}

fn stmts(ss: &[TStmt], v: &Vars) -> Option<Vec<FStmt>> {
    let mut out = Vec::with_capacity(ss.len());
    for st in ss {
        let next = match st {
            TStmt::Decl { slot, init: Some(e), .. } if *slot >= v.first_local && operand(v.locals[*slot as usize]).is_some() => FStmt::Decl { slot: *slot, init: expr(e, v)? },
            TStmt::If { c, then, els } => FStmt::If { c: expr(c, v)?, then: stmts(then, v)?, els: match els {
                Some(e) => stmts(e, v)?,
                None => Vec::new(),
            } },
            TStmt::Return(Some(e), _) => FStmt::Return(expr(e, v)?),
            TStmt::Return(None, _) => FStmt::Done,
            TStmt::Expr(e @ TExpr { k: TK::Assign { .. }, .. }) => assign(e, v)?,
            TStmt::Expr(TExpr { k: TK::CallContext { m, args, binding: None }, .. }) if args.len() == 1 => {
                let mi = registry().method(*m);
                let kind = match (mi.owner, &*mi.name) {
                    (cls::LONG_FIELD_SCRIPT | cls::DATE_FIELD_SCRIPT, "emit") => EmitTo::Long,
                    (cls::DOUBLE_FIELD_SCRIPT, "emit") => EmitTo::Double,
                    (cls::BOOLEAN_FIELD_SCRIPT, "emit") => EmitTo::Bool,
                    _ => return None,
                };
                FStmt::Emit { kind, arg: expr(&args[0], v)? }
            }
            TStmt::EachIter { slot, iterable, cast, body, .. } if *slot >= v.first_local => {
                let field = doc_field(iterable, v)?;
                let var = v.locals[*slot as usize];
                let conv = match operand(var)? {
                    Some(p) => Some(p),
                    None if cast.is_none() => None,
                    None => return None,
                };
                FStmt::EachDoc { field: field.into(), slot: *slot, conv, body: stmts(body, v)? }
            }
            _ => return None,
        };
        out.push(next);
    }
    Some(out)
}

/// `x = e`, `x op= e`, `x++`, `--x` of a primitive local whose value is not used.
fn assign(e: &TExpr, v: &Vars) -> Option<FStmt> {
    let TK::Assign { place, rhs, compound, read: false, .. } = &e.k else { return None };
    let Place::Local { slot } = &**place else { return None };
    if *slot < v.first_local || operand(v.locals[*slot as usize])?.is_none() {
        return None;
    }
    let value = match compound {
        None => expr(rhs, v)?,
        Some(c) => {
            // a `def` operation runs on the operands' specialized types, like `DefMath`
            let operand = operand(c.ty)?;
            if c.concat || !matches!(c.op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem) {
                return None;
            }
            let mut l = FExpr::Local(*slot);
            if let Some(up) = &c.up {
                if !numeric_cast(up) {
                    return None;
                }
                l = FExpr::Cast(Box::new(l), up.clone());
            }
            let mut out = FExpr::Bin { op: c.op, l: Box::new(l), r: Box::new(expr(rhs, v)?), operand, def: None };
            if let Some(down) = &c.down {
                if !numeric_cast(down) {
                    return None;
                }
                out = FExpr::Cast(Box::new(out), down.clone());
            }
            out
        }
    };
    Some(FStmt::Assign { slot: *slot, value })
}

struct Vars {
    score: Option<u32>,
    params: Option<u32>,
    doc: Option<u32>,
    /// Slots below are context variables.
    first_local: u32,
    locals: Vec<Ty>,
}

/// `doc['f']` with a literal field name.
fn doc_field<'e>(recv: &'e TExpr, v: &Vars) -> Option<&'e str> {
    let TK::Load(dp) = &recv.k else { return None };
    let Place::MapKey { recv: doc, key, .. } = &**dp else { return None };
    if local(doc).is_none() || local(doc) != v.doc {
        return None;
    }
    const_str(key)
}

fn local(e: &TExpr) -> Option<u32> {
    match &e.k {
        TK::Load(p) => match &**p {
            Place::Local { slot } => Some(*slot),
            _ => None,
        },
        _ => None,
    }
}

fn const_str(e: &TExpr) -> Option<&str> {
    match &e.k {
        TK::Const(Const::Str(s)) => Some(s),
        TK::Cast(x, Cast::Noop | Cast::Transparent) => const_str(x),
        _ => None,
    }
}

fn numeric_cast(c: &Cast) -> bool {
    match c {
        Cast::Num { to, .. } | Cast::DefTo { to, boxed: false, .. } => !matches!(to, Prim::Byte | Prim::Short | Prim::Char),
        Cast::Unbox(p) => !matches!(p, Prim::Byte | Prim::Short | Prim::Char),
        Cast::AsType(t) | Cast::MathResult { to: t, .. } => matches!(*t, Ty::INT | Ty::LONG | Ty::FLOAT | Ty::DOUBLE | Ty::BOOLEAN),
        Cast::Noop | Cast::Transparent => true,
        _ => false,
    }
}

fn operand(t: Ty) -> Option<Option<Prim>> {
    match t {
        Ty::DEF => Some(None),
        Ty::INT => Some(Some(Prim::Int)),
        Ty::LONG => Some(Some(Prim::Long)),
        Ty::FLOAT => Some(Some(Prim::Float)),
        Ty::DOUBLE => Some(Some(Prim::Double)),
        Ty::BOOLEAN => Some(Some(Prim::Bool)),
        _ => None,
    }
}

fn expr(e: &TExpr, v: &Vars) -> Option<FExpr> {
    let b = |x: &TExpr| expr(x, v).map(Box::new);
    Some(match &e.k {
        TK::Const(c) => match c {
            Const::Int(x) => FExpr::Int(*x),
            Const::Long(x) => FExpr::Long(*x),
            Const::Float(x) => FExpr::Float(*x),
            Const::Double(x) => FExpr::Double(*x),
            Const::Bool(x) => FExpr::Bool(*x),
            _ => return None,
        },
        TK::Cast(x, c) if numeric_cast(c) => FExpr::Cast(b(x)?, c.clone()),
        TK::Load(p) => match &**p {
            Place::Local { slot } if Some(*slot) == v.score => FExpr::Score,
            Place::Local { slot } if *slot >= v.first_local => FExpr::Local(*slot),
            Place::MapKey { recv, key, .. } if local(recv).is_some() && local(recv) == v.params => FExpr::Param(const_str(key)?.into()),
            Place::DefDot { recv, name, null_safe: false, result } if matches!(&**name, "value" | "empty") => {
                let f = doc_field(recv, v)?;
                let d = if &**name == "value" { FExpr::Doc(f.into()) } else { FExpr::DocEmpty(f.into()) };
                match result {
                    Some(c) if numeric_cast(c) => FExpr::Cast(Box::new(d), c.clone()),
                    Some(_) => return None,
                    None => d,
                }
            }
            _ => return None,
        },
        TK::Binary { op, l, r, operand: t, def } if matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem) => {
            let operand = operand(*t)?;
            if def.is_some_and(|d| d.compound) || operand == Some(Prim::Bool) {
                return None;
            }
            FExpr::Bin { op: *op, l: b(l)?, r: b(r)?, operand, def: *def }
        }
        TK::Unary { op: Op::Sub, x, def } => {
            let operand = if def.is_some() { None } else { Some(operand(e.ty)??) };
            if def.is_some_and(|d| d.compound) {
                return None;
            }
            FExpr::Neg { x: b(x)?, operand, def: *def }
        }
        TK::Unary { op: Op::Not, x, def: None } => FExpr::Not(b(x)?),
        TK::Comp { op, l, r, operand: t } if matches!(op, Op::Lt | Op::Lte | Op::Gt | Op::Gte | Op::Eq | Op::Ne) => {
            let operand = operand(*t)?;
            // `def ==` on two numbers compares them promoted, as primitives do (DefMath.eq);
            // a boolean against a number does not specialize (`promote` refuses it)
            FExpr::Cmp { op: *op, l: b(l)?, r: b(r)?, operand }
        }
        TK::And(l, r) => FExpr::And(b(l)?, b(r)?),
        TK::Or(l, r) => FExpr::Or(b(l)?, b(r)?),
        TK::Cond { c, t, f } => FExpr::Cond(b(c)?, b(t)?, b(f)?),
        TK::InvokeDyn { recv, name, args, null_safe: false, result } if matches!(&**name, "size" | "isEmpty") && args.is_empty() => {
            let f = doc_field(recv, v)?.into();
            let d = if &**name == "size" { FExpr::DocSize(f) } else { FExpr::DocEmpty(f) };
            match result {
                Some(c) if numeric_cast(c) => FExpr::Cast(Box::new(d), c.clone()),
                Some(_) => return None,
                None => d,
            }
        }
        TK::Invoke { recv: None, m, args, .. } => {
            let mi = registry().method(*m);
            if !(mi.owner == cls::MATH && MATH.contains(&&*mi.name)) || operand(mi.ret)?.is_none() {
                return None;
            }
            FExpr::Call { m: *m, args: args.iter().map(|a| expr(a, v)).collect::<Option<_>>()?, binding: None, ctx: false }
        }
        TK::CallContext { m, args, binding } => {
            let mi = registry().method(*m);
            if !SCORE_FNS.contains(&&*mi.name) || operand(mi.ret)?.is_none() {
                return None;
            }
            FExpr::Call { m: *m, args: args.iter().map(|a| expr(a, v)).collect::<Option<_>>()?, binding: *binding, ctx: true }
        }
        _ => return None,
    })
}

// ------------------------------------------------------------------------------ specialization

type Fx<T> = Box<dyn for<'v, 'a> Fn(&'v mut Vm<'a>) -> Result<T, ()>>;

/// Boxes a closure with the higher-ranked signature `Fx` needs (inference does not provide it
/// through `Box::new`).
fn fx<T, F>(f: F) -> Fx<T>
where
    F: for<'v, 'a> Fn(&'v mut Vm<'a>) -> Result<T, ()> + 'static,
{
    Box::new(f)
}

/// A specialized node: a closure producing an unboxed value of one type.
pub enum Typed {
    I(Fx<i32>),
    L(Fx<i64>),
    F(Fx<f32>),
    D(Fx<f64>),
    Z(Fx<bool>),
}

impl Typed {
    fn prim(&self) -> Prim {
        match self {
            Typed::I(_) => Prim::Int,
            Typed::L(_) => Prim::Long,
            Typed::F(_) => Prim::Float,
            Typed::D(_) => Prim::Double,
            Typed::Z(_) => Prim::Bool,
        }
    }
}

fn constant(v: &Value) -> Option<Typed> {
    Some(match *v {
        Value::Int(x) => Typed::I(fx(move |_| Ok(x))),
        Value::Long(x) => Typed::L(fx(move |_| Ok(x))),
        Value::Float(x) => Typed::F(fx(move |_| Ok(x))),
        Value::Double(x) => Typed::D(fx(move |_| Ok(x))),
        Value::Bool(x) => Typed::Z(fx(move |_| Ok(x))),
        _ => return None,
    })
}

/// Java's primitive conversion of `t` to `to` (widening and narrowing alike; booleans only to
/// themselves).
fn convert(t: Typed, to: Prim) -> Option<Typed> {
    use Typed::*;
    if t.prim() == to {
        return Some(t);
    }
    Some(match (t, to) {
        (I(f), Prim::Long) => L(fx(move |vm| f(vm).map(|x| x as i64))),
        (I(f), Prim::Float) => F(fx(move |vm| f(vm).map(|x| x as f32))),
        (I(f), Prim::Double) => D(fx(move |vm| f(vm).map(|x| x as f64))),
        (L(f), Prim::Int) => I(fx(move |vm| f(vm).map(|x| x as i32))),
        (L(f), Prim::Float) => F(fx(move |vm| f(vm).map(|x| x as f32))),
        (L(f), Prim::Double) => D(fx(move |vm| f(vm).map(|x| x as f64))),
        // Rust's float-to-int `as` saturates and maps NaN to 0, exactly like Java's d2i/d2l
        (F(f), Prim::Int) => I(fx(move |vm| f(vm).map(|x| x as i32))),
        (F(f), Prim::Long) => L(fx(move |vm| f(vm).map(|x| x as i64))),
        (F(f), Prim::Double) => D(fx(move |vm| f(vm).map(|x| x as f64))),
        (D(f), Prim::Int) => I(fx(move |vm| f(vm).map(|x| x as i32))),
        (D(f), Prim::Long) => L(fx(move |vm| f(vm).map(|x| x as i64))),
        (D(f), Prim::Float) => F(fx(move |vm| f(vm).map(|x| x as f32))),
        _ => return None,
    })
}

fn widens(from: Prim, to: Prim) -> bool {
    from == to || crate::cast::widens(from, to)
}

fn prim_of_ty(t: Ty) -> Option<Prim> {
    match t {
        Ty::INT => Some(Prim::Int),
        Ty::LONG => Some(Prim::Long),
        Ty::FLOAT => Some(Prim::Float),
        Ty::DOUBLE => Some(Prim::Double),
        Ty::BOOLEAN => Some(Prim::Bool),
        _ => None,
    }
}

/// `DefMath`'s promotion of two numeric operands.
fn promote(a: Prim, b: Prim) -> Option<Prim> {
    use Prim::*;
    Some(match (a, b) {
        (Bool, _) | (_, Bool) => return None,
        (Double, _) | (_, Double) => Double,
        (Float, _) | (_, Float) => Float,
        (Long, _) | (_, Long) => Long,
        _ => Int,
    })
}

struct Spec<'p> {
    params: Option<&'p Value>,
    doc_fields: &'p [Box<str>],
    /// The specialized type of each local slot declared so far (in program order).
    locals: std::cell::RefCell<Vec<Option<Prim>>>,
}

/// A compiled statement: `Some` when it returned.
type Fs = Box<dyn for<'v, 'a> Fn(&'v mut Vm<'a>) -> Result<Option<Ret>, ()>>;

/// The value a fast-path script returns.
#[derive(Clone)]
pub enum Ret {
    D(f64),
    Z(bool),
    /// A runtime field ran; its values are in `State::emitted`.
    Void,
    /// The boxed result of a `def`-returning script.
    Val(Value),
}

impl Spec<'_> {
    fn node(&self, e: &FExpr, vm: &mut Vm) -> Option<Typed> {
        use Typed::*;
        Some(match e {
            FExpr::Int(x) => constant(&Value::Int(*x))?,
            FExpr::Long(x) => constant(&Value::Long(*x))?,
            FExpr::Float(x) => constant(&Value::Float(*x))?,
            FExpr::Double(x) => constant(&Value::Double(*x))?,
            FExpr::Bool(x) => constant(&Value::Bool(*x))?,
            FExpr::Score => D(fx(|vm| Ok(vm.host.score()))),
            FExpr::Param(k) => {
                let Value::Map(m) = self.params? else { return None };
                if m.lazy_host && matches!(m.get_str(k), Some(Value::Obj(_))) {
                    return None;
                }
                constant(&m.get_str(k)?)?
            }
            FExpr::Local(slot) => {
                let slot = *slot as usize;
                match (*self.locals.borrow()).get(slot).copied().flatten()? {
                    Prim::Int => I(fx(move |vm| Ok(vm.st.fast_locals[slot] as u32 as i32))),
                    Prim::Long => L(fx(move |vm| Ok(vm.st.fast_locals[slot] as i64))),
                    Prim::Float => F(fx(move |vm| Ok(f32::from_bits(vm.st.fast_locals[slot] as u32)))),
                    Prim::Double => D(fx(move |vm| Ok(f64::from_bits(vm.st.fast_locals[slot])))),
                    Prim::Bool => Z(fx(move |vm| Ok(vm.st.fast_locals[slot] != 0))),
                    _ => return None,
                }
            }
            FExpr::DocSize(name) | FExpr::DocEmpty(name) => {
                let slot = self.doc_fields.iter().position(|f| **f == **name)?;
                let fetch = move |vm: &mut Vm| -> Result<(), ()> {
                    if vm.st.doc_buf_gen[slot] != vm.st.doc_gen {
                        crate::es::doc_fetch(vm, slot as u32).map_err(|_| ())?;
                    }
                    Ok(())
                };
                if matches!(e, FExpr::DocSize(_)) {
                    I(fx(move |vm| {
                        fetch(vm)?;
                        Ok(vm.st.doc_bufs[slot].len() as i32)
                    }))
                } else {
                    Z(fx(move |vm| {
                        fetch(vm)?;
                        Ok(vm.st.doc_bufs[slot].is_empty())
                    }))
                }
            }
            FExpr::Doc(name) => {
                let slot = self.doc_fields.iter().position(|f| **f == **name)?;
                // the field's kind, from the current document (fixed by the mapping)
                if vm.st.doc_buf_gen[slot] != vm.st.doc_gen {
                    crate::es::doc_fetch(vm, slot as u32).ok()?;
                }
                use crate::host::DocKind as K;
                let kind = vm.st.doc_bufs[slot].kind;
                let fetch = move |vm: &mut Vm| -> Result<(), ()> {
                    if vm.st.doc_buf_gen[slot] != vm.st.doc_gen {
                        crate::es::doc_fetch(vm, slot as u32).map_err(|_| ())?;
                    }
                    Ok(())
                };
                match kind {
                    K::Long | K::Integer | K::Short | K::Byte => L(fx(move |vm| {
                        fetch(vm)?;
                        let dv = &vm.st.doc_bufs[slot];
                        match dv.longs.first() {
                            Some(&x) if matches!(dv.kind, K::Long | K::Integer | K::Short | K::Byte) => Ok(x),
                            _ => Err(()),
                        }
                    })),
                    K::Double | K::Float | K::HalfFloat | K::ScaledFloat => D(fx(move |vm| {
                        fetch(vm)?;
                        let dv = &vm.st.doc_bufs[slot];
                        match dv.doubles.first() {
                            Some(&x) if matches!(dv.kind, K::Double | K::Float | K::HalfFloat | K::ScaledFloat) => Ok(x),
                            _ => Err(()),
                        }
                    })),
                    K::Boolean => Z(fx(move |vm| {
                        fetch(vm)?;
                        let dv = &vm.st.doc_bufs[slot];
                        match dv.longs.first() {
                            Some(&x) if dv.kind == K::Boolean => Ok(x != 0),
                            _ => Err(()),
                        }
                    })),
                    _ => return None,
                }
            }
            FExpr::Cast(x, c) => {
                // `cond ? 0 : doc['f'].value` is a `def` of mixed boxed types converted as a
                // whole: converting each branch is the same (each one widens on its own)
                if let FExpr::Cond(cc, t, f) = &**x
                    && matches!(c, Cast::DefTo { .. } | Cast::AsType(_))
                    && let Some(mixed) = self.mixed_cond(cc, t, f, c, vm)
                {
                    return Some(mixed);
                }
                let t = self.node(x, vm)?;
                let from = t.prim();
                match c {
                    Cast::Noop | Cast::Transparent | Cast::Unbox(_) => t,
                    Cast::Num { to, .. } => convert(t, *to)?,
                    Cast::DefTo { to, explicit, .. } => {
                        if *explicit && from != Prim::Bool && *to != Prim::Bool || widens(from, *to) {
                            convert(t, *to)?
                        } else {
                            return None;
                        }
                    }
                    Cast::AsType(ty) => {
                        let to = prim_of_ty(*ty)?;
                        if !widens(from, to) {
                            return None;
                        }
                        convert(t, to)?
                    }
                    Cast::MathResult { to, explicit } => self.math_result(t, *to, *explicit)?,
                    _ => return None,
                }
            }
            FExpr::Bin { op, l, r, operand, def } => {
                let (l, r) = (self.node(l, vm)?, self.node(r, vm)?);
                let p = match operand {
                    Some(p) => *p,
                    None if l.prim() == Prim::Bool && r.prim() == Prim::Bool => Prim::Bool,
                    None => promote(l.prim(), r.prim())?,
                };
                let (l, r) = (convert(l, p)?, convert(r, p)?);
                let out = arith(*op, l, r)?;
                match def {
                    Some(d) if d.ret != Ty::DEF => self.math_result(out, d.ret, d.explicit)?,
                    _ => out,
                }
            }
            FExpr::Neg { x, operand, def } => {
                let t = self.node(x, vm)?;
                let t = match operand {
                    Some(p) => convert(t, *p)?,
                    // DefMath promotes `byte`/`short`/`char` to int; the fast path has none
                    None => t,
                };
                let out = match t {
                    I(f) => I(fx(move |vm| f(vm).map(i32::wrapping_neg))),
                    L(f) => L(fx(move |vm| f(vm).map(i64::wrapping_neg))),
                    F(f) => F(fx(move |vm| f(vm).map(|x| -x))),
                    D(f) => D(fx(move |vm| f(vm).map(|x| -x))),
                    Z(_) => return None,
                };
                match def {
                    Some(d) if d.ret != Ty::DEF => self.math_result(out, d.ret, d.explicit)?,
                    _ => out,
                }
            }
            FExpr::Cmp { op, l, r, operand } => {
                let (l, r) = (self.node(l, vm)?, self.node(r, vm)?);
                let p = match operand {
                    Some(p) => *p,
                    None if l.prim() == Prim::Bool && r.prim() == Prim::Bool => Prim::Bool,
                    None => promote(l.prim(), r.prim())?,
                };
                compare(*op, convert(l, p)?, convert(r, p)?)?
            }
            FExpr::Not(x) => match self.node(x, vm)? {
                Z(f) => Z(fx(move |vm| f(vm).map(|b| !b))),
                _ => return None,
            },
            FExpr::And(l, r) => match (self.node(l, vm)?, self.node(r, vm)?) {
                (Z(a), Z(b)) => Z(fx(move |vm| Ok(a(vm)? && b(vm)?))),
                _ => return None,
            },
            FExpr::Or(l, r) => match (self.node(l, vm)?, self.node(r, vm)?) {
                (Z(a), Z(b)) => Z(fx(move |vm| Ok(a(vm)? || b(vm)?))),
                _ => return None,
            },
            FExpr::Cond(c, t, f) => {
                let Z(c) = self.node(c, vm)? else { return None };
                match (self.node(t, vm)?, self.node(f, vm)?) {
                    (I(a), I(b)) => I(fx(move |vm| if c(vm)? { a(vm) } else { b(vm) })),
                    (L(a), L(b)) => L(fx(move |vm| if c(vm)? { a(vm) } else { b(vm) })),
                    (F(a), F(b)) => F(fx(move |vm| if c(vm)? { a(vm) } else { b(vm) })),
                    (D(a), D(b)) => D(fx(move |vm| if c(vm)? { a(vm) } else { b(vm) })),
                    (Z(a), Z(b)) => Z(fx(move |vm| if c(vm)? { a(vm) } else { b(vm) })),
                    _ => return None,
                }
            }
            FExpr::Call { m, args, binding, ctx } => self.call(*m, args, *binding, *ctx, vm)?,
        })
    }

    /// A `def` conditional whose branches specialize to different types, under a conversion
    /// both branches accept: the conversion moves into the branches.
    fn mixed_cond(&self, c: &FExpr, t: &FExpr, f: &FExpr, cast: &Cast, vm: &mut Vm) -> Option<Typed> {
        let (tp, fp) = (self.node(t, vm)?.prim(), self.node(f, vm)?.prim());
        if tp == fp {
            return None;
        }
        let pushed = FExpr::Cond(Box::new(c.clone()), Box::new(FExpr::Cast(Box::new(t.clone()), cast.clone())), Box::new(FExpr::Cast(Box::new(f.clone()), cast.clone())));
        self.node(&pushed, vm)
    }

    /// The conversion of a `def` operator's result to its call-site type (`cast_math_result`).
    fn math_result(&self, t: Typed, to: Ty, explicit: bool) -> Option<Typed> {
        if to == Ty::DEF {
            return Some(t);
        }
        let to = prim_of_ty(to)?;
        let from = t.prim();
        if from == to {
            return Some(t);
        }
        if explicit {
            if from == Prim::Bool || to == Prim::Bool {
                return None;
            }
            return convert(t, to);
        }
        if widens(from, to) {
            return convert(t, to);
        }
        None
    }

    /// A library call: arguments evaluated unboxed, passed to the native as values (`Math`
    /// functions over doubles are called directly).
    fn call(&self, m: MethodId, args: &[FExpr], binding: Option<u32>, ctx: bool, vm: &mut Vm) -> Option<Typed> {
        let mi = registry().method(m);
        let imp = mi.imp;
        let ret = prim_of_ty(mi.ret)?;
        let mut typed = Vec::with_capacity(args.len());
        for (a, p) in args.iter().zip(&mi.params) {
            let t = self.node(a, vm)?;
            // arguments arrive cast to the parameter types
            if Some(t.prim()) != prim_of_ty(*p) {
                return None;
            }
            typed.push(t);
        }
        if !ctx && ret == Prim::Double && typed.iter().all(|t| t.prim() == Prim::Double) {
            use crate::lib_lang::MathFn;
            let mut it = typed.into_iter();
            match (crate::lib_lang::math_fn(&mi.name, mi.params.len(), mi.owner == cls::STRICT_MATH), it.next(), it.next()) {
                (Some(MathFn::F1(f)), Some(Typed::D(a)), None) => return Some(Typed::D(fx(move |vm| Ok(f(a(vm)?))))),
                (Some(MathFn::F2(f)), Some(Typed::D(a)), Some(Typed::D(b))) => return Some(Typed::D(fx(move |vm| Ok(f(a(vm)?, b(vm)?))))),
                (_, a, b) => typed = a.into_iter().chain(b).chain(it).collect(),
            }
        }
        let binding = binding.unwrap_or(u32::MAX);
        macro_rules! run {
            ($n:expr, $build:expr) => {{
                let typed = typed;
                let f: Fx<Value> = fx(move |vm| {
                    let mut a: [Value; $n] = $build(&typed, vm)?;
                    if ctx {
                        vm.st.cur_binding = binding;
                    }
                    imp(vm, &mut a).map_err(|_| ())
                });
                f
            }};
        }
        fn val(t: &Typed, vm: &mut Vm) -> Result<Value, ()> {
            Ok(match t {
                Typed::I(f) => Value::Int(f(vm)?),
                Typed::L(f) => Value::Long(f(vm)?),
                Typed::F(f) => Value::Float(f(vm)?),
                Typed::D(f) => Value::Double(f(vm)?),
                Typed::Z(f) => Value::Bool(f(vm)?),
            })
        }
        let f: Fx<Value> = match typed.len() {
            1 => run!(1, |t: &Vec<Typed>, vm: &mut Vm| -> Result<[Value; 1], ()> { Ok([val(&t[0], vm)?]) }),
            2 => run!(2, |t: &Vec<Typed>, vm: &mut Vm| -> Result<[Value; 2], ()> { Ok([val(&t[0], vm)?, val(&t[1], vm)?]) }),
            3 => run!(3, |t: &Vec<Typed>, vm: &mut Vm| -> Result<[Value; 3], ()> { Ok([val(&t[0], vm)?, val(&t[1], vm)?, val(&t[2], vm)?]) }),
            4 => run!(4, |t: &Vec<Typed>, vm: &mut Vm| -> Result<[Value; 4], ()> {
                Ok([val(&t[0], vm)?, val(&t[1], vm)?, val(&t[2], vm)?, val(&t[3], vm)?])
            }),
            5 => run!(5, |t: &Vec<Typed>, vm: &mut Vm| -> Result<[Value; 5], ()> {
                Ok([val(&t[0], vm)?, val(&t[1], vm)?, val(&t[2], vm)?, val(&t[3], vm)?, val(&t[4], vm)?])
            }),
            _ => return None,
        };
        Some(match ret {
            Prim::Int => Typed::I(fx(move |vm| match f(vm)? {
                Value::Int(x) => Ok(x),
                _ => Err(()),
            })),
            Prim::Long => Typed::L(fx(move |vm| match f(vm)? {
                Value::Long(x) => Ok(x),
                _ => Err(()),
            })),
            Prim::Float => Typed::F(fx(move |vm| match f(vm)? {
                Value::Float(x) => Ok(x),
                _ => Err(()),
            })),
            Prim::Double => Typed::D(fx(move |vm| match f(vm)? {
                Value::Double(x) => Ok(x),
                _ => Err(()),
            })),
            Prim::Bool => Typed::Z(fx(move |vm| match f(vm)? {
                Value::Bool(x) => Ok(x),
                _ => Err(()),
            })),
            _ => return None,
        })
    }
}

fn arith(op: Op, l: Typed, r: Typed) -> Option<Typed> {
    use Typed::*;
    macro_rules! bin {
        ($ctor:ident, $a:expr, $b:expr, |$x:ident, $y:ident| $body:expr) => {{
            let (a, b) = ($a, $b);
            $ctor(fx(move |vm| {
                let ($x, $y) = (a(vm)?, b(vm)?);
                $body
            }))
        }};
    }
    Some(match (l, r) {
        (I(a), I(b)) => match op {
            Op::Add => bin!(I, a, b, |x, y| Ok(x.wrapping_add(y))),
            Op::Sub => bin!(I, a, b, |x, y| Ok(x.wrapping_sub(y))),
            Op::Mul => bin!(I, a, b, |x, y| Ok(x.wrapping_mul(y))),
            // a zero divisor throws: the interpreter reports it
            Op::Div => bin!(I, a, b, |x, y| if y == 0 { Err(()) } else { Ok(x.wrapping_div(y)) }),
            Op::Rem => bin!(I, a, b, |x, y| if y == 0 { Err(()) } else { Ok(x.wrapping_rem(y)) }),
            _ => return None,
        },
        (L(a), L(b)) => match op {
            Op::Add => bin!(L, a, b, |x, y| Ok(x.wrapping_add(y))),
            Op::Sub => bin!(L, a, b, |x, y| Ok(x.wrapping_sub(y))),
            Op::Mul => bin!(L, a, b, |x, y| Ok(x.wrapping_mul(y))),
            Op::Div => bin!(L, a, b, |x, y| if y == 0 { Err(()) } else { Ok(x.wrapping_div(y)) }),
            Op::Rem => bin!(L, a, b, |x, y| if y == 0 { Err(()) } else { Ok(x.wrapping_rem(y)) }),
            _ => return None,
        },
        (F(a), F(b)) => match op {
            Op::Add => bin!(F, a, b, |x, y| Ok(x + y)),
            Op::Sub => bin!(F, a, b, |x, y| Ok(x - y)),
            Op::Mul => bin!(F, a, b, |x, y| Ok(x * y)),
            Op::Div => bin!(F, a, b, |x, y| Ok(x / y)),
            Op::Rem => bin!(F, a, b, |x, y| Ok(x % y)),
            _ => return None,
        },
        (D(a), D(b)) => match op {
            Op::Add => bin!(D, a, b, |x, y| Ok(x + y)),
            Op::Sub => bin!(D, a, b, |x, y| Ok(x - y)),
            Op::Mul => bin!(D, a, b, |x, y| Ok(x * y)),
            Op::Div => bin!(D, a, b, |x, y| Ok(x / y)),
            Op::Rem => bin!(D, a, b, |x, y| Ok(x % y)),
            _ => return None,
        },
        _ => return None,
    })
}

fn compare(op: Op, l: Typed, r: Typed) -> Option<Typed> {
    use Typed::*;
    macro_rules! cmp {
        ($a:expr, $b:expr) => {{
            let (a, b) = ($a, $b);
            Z(match op {
                Op::Lt => fx(move |vm| Ok(a(vm)? < b(vm)?)),
                Op::Lte => fx(move |vm| Ok(a(vm)? <= b(vm)?)),
                Op::Gt => fx(move |vm| Ok(a(vm)? > b(vm)?)),
                Op::Gte => fx(move |vm| Ok(a(vm)? >= b(vm)?)),
                Op::Eq => fx(move |vm| Ok(a(vm)? == b(vm)?)),
                Op::Ne => fx(move |vm| Ok(a(vm)? != b(vm)?)),
                _ => return None,
            })
        }};
    }
    Some(match (l, r) {
        (I(a), I(b)) => cmp!(a, b),
        (L(a), L(b)) => cmp!(a, b),
        (F(a), F(b)) => cmp!(a, b),
        (D(a), D(b)) => cmp!(a, b),
        (Z(a), Z(b)) if matches!(op, Op::Eq | Op::Ne) => cmp!(a, b),
        _ => return None,
    })
}

impl Spec<'_> {
    /// Compiles a statement list as a chain: each statement's closure runs the rest itself,
    /// so a declaration costs one call and no loop.
    /// `ret`: the primitive `execute` returns, `None` for `def` (the value is boxed).
    fn stmts(&self, body: &[FStmt], ret: Option<Prim>, vm: &mut Vm) -> Option<Fs> {
        let Some((first, rest)) = body.split_first() else {
            return Some(Box::new(|_| Ok(None)));
        };
        Some(match first {
            FStmt::Decl { slot, init } => {
                let t = self.node(init, vm)?;
                let s = *slot as usize;
                {
                    let mut l = self.locals.borrow_mut();
                    if l.len() <= s {
                        l.resize(s + 1, None);
                    }
                    l[s] = Some(t.prim());
                }
                decl(t, s, self.stmts(rest, ret, vm)?)
            }
            FStmt::If { c, then, els } => {
                let Typed::Z(c) = self.node(c, vm)? else { return None };
                if returns(then) && els.is_empty() && !rest.is_empty() {
                    // `if (c) { ... return } rest`: the else arm is the rest
                    let (t, rest) = (self.stmts(then, ret, vm)?, self.stmts(rest, ret, vm)?);
                    return Some(Box::new(move |vm| if c(vm)? { t(vm) } else { rest(vm) }));
                }
                let (t, e) = (self.stmts(then, ret, vm)?, self.stmts(els, ret, vm)?);
                if rest.is_empty() {
                    Box::new(move |vm| if c(vm)? { t(vm) } else { e(vm) })
                } else {
                    let rest = self.stmts(rest, ret, vm)?;
                    Box::new(move |vm| match if c(vm)? { t(vm)? } else { e(vm)? } {
                        Some(r) => Ok(Some(r)),
                        None => rest(vm),
                    })
                }
            }
            FStmt::Return(e) => match (self.node(e, vm)?, ret) {
                (Typed::D(f), Some(Prim::Double)) => Box::new(move |vm| Ok(Some(Ret::D(f(vm)?)))),
                (Typed::Z(f), Some(Prim::Bool)) => Box::new(move |vm| Ok(Some(Ret::Z(f(vm)?)))),
                (Typed::I(f), None) => Box::new(move |vm| Ok(Some(Ret::Val(Value::Int(f(vm)?))))),
                (Typed::L(f), None) => Box::new(move |vm| Ok(Some(Ret::Val(Value::Long(f(vm)?))))),
                (Typed::F(f), None) => Box::new(move |vm| Ok(Some(Ret::Val(Value::Float(f(vm)?))))),
                (Typed::D(f), None) => Box::new(move |vm| Ok(Some(Ret::Val(Value::Double(f(vm)?))))),
                (Typed::Z(f), None) => Box::new(move |vm| Ok(Some(Ret::Val(Value::Bool(f(vm)?))))),
                _ => return None,
            },
            FStmt::Done => Box::new(|_| Ok(Some(Ret::Void))),
            FStmt::Assign { slot, value } => {
                let s = *slot as usize;
                // the local keeps the type its declaration gave it
                let want = (*self.locals.borrow()).get(s).copied().flatten()?;
                let t = self.node(value, vm)?;
                if t.prim() != want {
                    return None;
                }
                decl(t, s, self.stmts(rest, ret, vm)?)
            }
            FStmt::EachDoc { field, slot, conv, body } => {
                use crate::host::DocKind as K;
                let fslot = self.doc_fields.iter().position(|f| **f == **field)?;
                if vm.st.doc_buf_gen[fslot] != vm.st.doc_gen {
                    crate::es::doc_fetch(vm, fslot as u32).ok()?;
                }
                let kind = vm.st.doc_bufs[fslot].kind;
                let elem = match kind {
                    K::Long | K::Integer | K::Short | K::Byte => Prim::Long,
                    K::Double | K::Float | K::HalfFloat | K::ScaledFloat => Prim::Double,
                    K::Boolean => Prim::Bool,
                    _ => return None,
                };
                let var = conv.unwrap_or(elem);
                // `ValueIterator`'s conversions: numbers to any numeric type, booleans only to
                // boolean
                let read: fn(&crate::host::DocValues, usize) -> u64 = match (elem, var) {
                    (Prim::Long, Prim::Long) => |dv, i| dv.longs[i] as u64,
                    (Prim::Long, Prim::Int) => |dv, i| dv.longs[i] as i32 as u32 as u64,
                    (Prim::Long, Prim::Double) => |dv, i| (dv.longs[i] as f64).to_bits(),
                    (Prim::Long, Prim::Float) => |dv, i| (dv.longs[i] as f32).to_bits() as u64,
                    (Prim::Double, Prim::Double) => |dv, i| dv.doubles[i].to_bits(),
                    (Prim::Double, Prim::Float) => |dv, i| (dv.doubles[i] as f32).to_bits() as u64,
                    (Prim::Double, Prim::Long) => |dv, i| dv.doubles[i] as i64 as u64,
                    (Prim::Double, Prim::Int) => |dv, i| dv.doubles[i] as i32 as u32 as u64,
                    (Prim::Bool, Prim::Bool) => |dv, i| u64::from(dv.longs[i] != 0),
                    _ => return None,
                };
                let s = *slot as usize;
                {
                    let mut l = self.locals.borrow_mut();
                    if l.len() <= s {
                        l.resize(s + 1, None);
                    }
                    l[s] = Some(var);
                }
                let (b, rest) = (self.stmts(body, ret, vm)?, self.stmts(rest, ret, vm)?);
                Box::new(move |vm| {
                    if vm.st.doc_buf_gen[fslot] != vm.st.doc_gen {
                        crate::es::doc_fetch(vm, fslot as u32).map_err(|_| ())?;
                    }
                    if vm.st.doc_bufs[fslot].kind != kind {
                        return Err(());
                    }
                    let n = vm.st.doc_bufs[fslot].len();
                    for i in 0..n {
                        vm.st.fast_locals[s] = read(&vm.st.doc_bufs[fslot], i);
                        if let Some(r) = b(vm)? {
                            return Ok(Some(r));
                        }
                    }
                    rest(vm)
                })
            }
            FStmt::Emit { kind, arg } => {
                let t = self.node(arg, vm)?;
                let rest = self.stmts(rest, ret, vm)?;
                // the argument converts to the parameter type implicitly: widening only
                let push: Fx<Value> = match (*kind, t) {
                    (EmitTo::Long, Typed::I(f)) => fx(move |vm| f(vm).map(|x| Value::Long(x as i64))),
                    (EmitTo::Long, Typed::L(f)) => fx(move |vm| f(vm).map(Value::Long)),
                    (EmitTo::Double, Typed::I(f)) => fx(move |vm| f(vm).map(|x| Value::Double(x as f64))),
                    (EmitTo::Double, Typed::L(f)) => fx(move |vm| f(vm).map(|x| Value::Double(x as f64))),
                    (EmitTo::Double, Typed::F(f)) => fx(move |vm| f(vm).map(|x| Value::Double(x as f64))),
                    (EmitTo::Double, Typed::D(f)) => fx(move |vm| f(vm).map(Value::Double)),
                    (EmitTo::Bool, Typed::Z(f)) => fx(move |vm| f(vm).map(Value::Bool)),
                    _ => return None,
                };
                Box::new(move |vm| {
                    let v = push(vm)?;
                    // past the per-document limit the interpreter reports it
                    if vm.st.emitted.len() >= crate::es::emit::MAX_VALUES {
                        return Err(());
                    }
                    vm.st.emitted.push(v);
                    rest(vm)
                })
            }
        })
    }
}

/// A declaration storing its value's bits in `State::fast_locals`, then the rest.
fn decl(t: Typed, slot: usize, rest: Fs) -> Fs {
    macro_rules! store {
        ($f:expr, $bits:expr) => {{
            let f = $f;
            Box::new(move |vm| {
                let x = f(vm)?;
                vm.st.fast_locals[slot] = $bits(x);
                rest(vm)
            })
        }};
    }
    match t {
        Typed::I(f) => store!(f, |x: i32| x as u32 as u64),
        Typed::L(f) => store!(f, |x: i64| x as u64),
        Typed::F(f) => store!(f, |x: f32| x.to_bits() as u64),
        Typed::D(f) => store!(f, f64::to_bits),
        Typed::Z(f) => store!(f, u64::from),
    }
}

/// A specialized script: runs one document, `Err` to hand it to the interpreter.
pub struct Compiled {
    body: Fs,
    /// A runtime field: falling off the end is the result.
    void: bool,
}

impl Compiled {
    #[inline]
    pub fn run(&self, vm: &mut Vm) -> Option<Ret> {
        if self.void {
            vm.st.emitted.clear();
            return match (self.body)(vm) {
                Ok(_) => Some(Ret::Void),
                Err(()) => None,
            };
        }
        (self.body)(vm).ok().flatten()
    }
}

/// Specializes a plan for an instance (`params` as bound, doc kinds read from the current
/// document); `None` when some value is not of a supported type.
pub fn specialize(plan: &Plan, params: Option<&Value>, ret: Ty, vm: &mut Vm) -> Option<Compiled> {
    let prog = vm.prog;
    let spec = Spec { params, doc_fields: &prog.doc_fields, locals: std::cell::RefCell::new(Vec::new()) };
    let void = ret == Ty::VOID;
    let ret = if void {
        Some(Prim::Bool)
    } else if ret == Ty::DEF {
        None
    } else {
        Some(prim_of_ty(ret)?)
    };
    let max_slot = plan_slots(&plan.body);
    if vm.st.fast_locals.len() < max_slot {
        vm.st.fast_locals.resize(max_slot, 0);
    }
    spec.stmts(&plan.body, ret, vm).map(|body| Compiled { body, void })
}

fn plan_slots(b: &[FStmt]) -> usize {
    b.iter()
        .map(|s| match s {
            FStmt::Decl { slot, .. } => *slot as usize + 1,
            FStmt::If { then, els, .. } => plan_slots(then).max(plan_slots(els)),
            FStmt::Assign { slot, .. } => *slot as usize + 1,
            FStmt::EachDoc { slot, body, .. } => (*slot as usize + 1).max(plan_slots(body)),
            FStmt::Return(_) | FStmt::Emit { .. } | FStmt::Done => 0,
        })
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use crate::{Context, DocKind, DocValues, Engine, Host, HostError, Settings, Value};

    /// `def` results (aggregation scripts, script fields): the same boxed value and class.
    #[test]
    fn boxed_results_agree_with_the_interpreter() {
        let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
        let params = serde_json::json!({ "f": 1.5, "n": 3 });
        let scripts = [
            "doc['l'].value * 2",
            "doc['d'].value + 1",
            "doc['l'].value > 3",
            "doc['i'].value",
            "doc['i'].value * params.n",
            "(float) doc['d'].value",
            "(int) doc['d'].value",
            "params.n",
            "params.f * doc['l'].value",
            "doc['e'].value",
            "doc['b'].value",
            "doc['l'].size()",
            "doc['l'].value / 0",
            "Math.max(doc['l'].value, 2)",
            "long x = doc['l'].value; if (x > 0) { return x } return -x",
            "long s = 0; for (long v : doc['l']) { s += v } return s",
        ];
        let docs = [Doc { long: 7, double: 0.25 }, Doc { long: -3, double: -2.5 }, Doc { long: i64::MAX, double: 1e300 }];
        for ctx in [Context::Aggs, Context::Field] {
            for src in scripts {
                let script = engine.compile(src, ctx).unwrap_or_else(|e| panic!("{src}: {e:?}"));
                let (mut fast, mut slow) = (script.instance(), script.instance());
                fast.set_params(&params);
                slow.set_params(&params);
                slow.disable_fast_path();
                for d in &docs {
                    let mut h1 = Doc { long: d.long, double: d.double };
                    let mut h2 = Doc { long: d.long, double: d.double };
                    let (a, b) = (fast.execute(&mut h1), slow.execute(&mut h2));
                    assert!(same(&a, &b), "{src}: fast {a:?} vs interpreter {b:?}");
                    if let (Ok(x), Ok(y)) = (&a, &b) {
                        assert_eq!(x.class(), y.class(), "{src}");
                    }
                }
                if !src.contains("doc['e']") {
                    assert!(fast.fast_path_active(), "{} {src} should take the fast path", ctx.name());
                }
            }
        }
    }

    /// Runtime fields: the fast path emits what the interpreter emits, or bails out.
    #[test]
    fn runtime_fields_agree_with_the_interpreter() {
        let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
        let params = serde_json::json!({ "f": 1.5, "n": 3 });
        let cases = [
            (Context::DoubleField, "emit(doc['d'].value * 1.1)", true),
            (Context::DoubleField, "emit(doc['l'].value + params.f)", true),
            (Context::DoubleField, "if (doc['l'].value > 0) { emit(doc['d'].value) } else { emit(-1) }", true),
            (Context::DoubleField, "emit(doc['d'].value); emit(doc['l'].value)", true),
            (Context::DoubleField, "if (doc['e'].size() > 0) { emit(doc['e'].value) }", true),
            (Context::LongField, "emit(doc['l'].value * params.n)", true),
            (Context::LongField, "emit(doc['i'].value + 1)", true),
            (Context::LongField, "emit((long) doc['d'].value)", true),
            (Context::LongField, "emit(doc['l'].value / 0)", true),
            (Context::DateField, "emit(doc['l'].value + 86400000L)", true),
            (Context::BooleanField, "emit(doc['l'].value > 3)", true),
            (Context::BooleanField, "emit(doc['b'].value)", true),
            (Context::LongField, "for (long v : doc['l']) { emit(v * 2) }", true),
            (Context::DoubleField, "for (double v : doc['d']) { emit(v); emit(v + 1) }", true),
            (Context::LongField, "for (int i = 0; i < 150; ++i) { emit(i) }", false),
            (Context::LongField, "for (long v : doc['l']) { for (long w : doc['l']) { emit(v * w) } }", true),
            (Context::LongField, "emit(doc['missing'].value)", false),
            (Context::KeywordField, "emit('x')", false),
        ];
        let docs = [Doc { long: 7, double: 0.25 }, Doc { long: -3, double: -2.5 }, Doc { long: i64::MAX, double: 1e300 }];
        for (ctx, src, fast_expected) in cases {
            let script = engine.compile(src, ctx).unwrap_or_else(|e| panic!("{src}: {e:?}"));
            let (mut fast, mut slow) = (script.instance(), script.instance());
            fast.set_params(&params);
            slow.set_params(&params);
            slow.disable_fast_path();
            for d in &docs {
                let mut h1 = Doc { long: d.long, double: d.double };
                let mut h2 = Doc { long: d.long, double: d.double };
                let (a, b) = (fast.execute(&mut h1), slow.execute(&mut h2));
                assert!(same(&a, &b), "{src}: fast {a:?} vs interpreter {b:?}");
                let (ea, eb) = (fast.emitted().to_vec(), slow.emitted().to_vec());
                assert!(ea.len() == eb.len() && ea.iter().zip(&eb).all(|(x, y)| x.java_equals(y) && x.class() == y.class()), "{src}: {ea:?} vs {eb:?}");
            }
            assert_eq!(fast.fast_path_active(), fast_expected, "{src}");
        }
    }

    struct Doc {
        long: i64,
        double: f64,
    }

    impl Host for Doc {
        fn score(&mut self) -> f64 {
            1.75
        }
        fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError> {
            match field {
                "l" => {
                    out.clear(DocKind::Long);
                    out.longs.push(self.long);
                }
                "i" => {
                    out.clear(DocKind::Integer);
                    out.longs.push(self.long);
                }
                "d" => {
                    out.clear(DocKind::Double);
                    out.doubles.push(self.double);
                }
                "b" => {
                    out.clear(DocKind::Boolean);
                    out.longs.push(self.long & 1);
                }
                "e" => out.clear(DocKind::Long),
                _ => return Ok(false),
            }
            Ok(true)
        }
    }

    fn same(a: &Result<Value, crate::ScriptError>, b: &Result<Value, crate::ScriptError>) -> bool {
        match (a, b) {
            (Ok(x), Ok(y)) => x.java_equals(y) || (x.as_f64().is_some_and(f64::is_nan) && y.as_f64().is_some_and(f64::is_nan)),
            (Err(x), Err(y)) => x.to_json() == y.to_json(),
            _ => false,
        }
    }

    /// `return e` → `return (e) > 1`, for the boolean-returning filter context.
    fn as_filter(body: &str) -> String {
        let mut out = String::new();
        let mut rest = body;
        while let Some(i) = rest.find("return ") {
            out.push_str(&rest[..i + 7]);
            rest = &rest[i + 7..];
            let end = rest.find([';', '}']).unwrap_or(rest.len());
            out.push_str(&format!("({}) > 1", rest[..end].trim_end()));
            out.push(' ');
            rest = &rest[end..];
        }
        out + rest
    }

    /// Every script gives the fast path's and the interpreter's answer for every document.
    #[test]
    fn fast_path_agrees_with_the_interpreter() {
        let engine = Engine::new(Settings { max_compilations_rate: None, ..Settings::default() });
        let math: Vec<String> = ["abs", "acos", "asin", "atan", "cbrt", "ceil", "cos", "cosh", "exp", "expm1", "floor", "log", "log10", "log1p", "rint", "signum", "sin", "sinh", "sqrt", "tan", "tanh", "toDegrees", "toRadians"]
            .iter()
            .map(|f| format!("Math.{f}(doc['d'].value)"))
            .chain(["atan2", "hypot", "max", "min", "pow"].iter().map(|f| format!("Math.{f}(doc['d'].value, params.f)")))
            .collect();
        let mut scripts: Vec<String> = vec![
            "1.0".into(),
            "_score".into(),
            "params.f".into(),
            "params.n".into(),
            "doc['l'].value".into(),
            "doc['i'].value * 3".into(),
            "doc['l'].value / params.n".into(),
            "doc['l'].value % 7 - params.n".into(),
            "doc['d'].value / doc['l'].value".into(),
            "doc['l'].value * 1000000000000L".into(),
            "-doc['l'].value".into(),
            "-doc['d'].value + params.f".into(),
            "_score * params.f + doc['d'].value".into(),
            "doc['l'].value > 5 ? doc['d'].value : -1.0".into(),
            "doc['b'].value ? 1 : 0".into(),
            "doc['e'].value".into(),
            "doc['missing'].value".into(),
            "(float) doc['d'].value * 2.5f".into(),
            "(int) doc['d'].value".into(),
            "(long) params.f * 7".into(),
            "doc['l'].value / 0".into(),
            "params.n / (doc['l'].value - doc['l'].value)".into(),
            "saturation(doc['d'].value, 10)".into(),
            "sigmoid(doc['d'].value, 2, 0.5)".into(),
            "decayNumericGauss(10, 5, 0, 0.5, doc['d'].value) * _score".into(),
            "decayNumericExp(params.o, 5, 0, 0.5, doc['l'].value)".into(),
            "decayNumericLinear(10, -5, 0, 0.5, doc['d'].value)".into(),
            "Math.log(2 + doc['l'].value) * params.f".into(),
        ];
        scripts.extend(math);
        let params = serde_json::json!({ "f": 1.5, "n": 3, "o": 4 });
        let docs = [Doc { long: 7, double: 0.25 }, Doc { long: -3, double: -2.5 }, Doc { long: 0, double: f64::NAN }, Doc { long: i64::MAX, double: 1e300 }];
        let (mut active, mut compiled) = (0, 0);
        for (ctx, wrap) in [(Context::Score, false), (Context::Filter, true)] {
            for s in &scripts {
                let src = if wrap { format!("return ({s}) > 1") } else { format!("return {s}") };
                let Ok(script) = engine.compile(&src, ctx) else { continue };
                compiled += 1;
                let (mut fast, mut slow) = (script.instance(), script.instance());
                fast.set_params(&params);
                slow.set_params(&params);
                slow.disable_fast_path();
                for d in &docs {
                    let mut h1 = Doc { long: d.long, double: d.double };
                    let mut h2 = Doc { long: d.long, double: d.double };
                    let (a, b) = (fast.execute(&mut h1), slow.execute(&mut h2));
                    assert!(same(&a, &b), "{src} on {}/{}: fast {a:?} vs interpreter {b:?}", d.long, d.double);
                }
                active += usize::from(fast.fast_path_active());

            }
        }
        // everything but the unmapped field specializes
        assert_eq!(active, compiled - 2, "scripts that took the fast path");

        // statement bodies: locals, branches, early returns
        let bodies = [
            "def x = doc['l'].value; if (x > 5) { return x / 2.0 } return x * params.f",
            "double s = _score; long n = doc['l'].value; if (n < 0) { return -s } else if (n == 0) { return 0 } else { return s * n }",
            "int k = params.n; float f = 2.5f; return doc['d'].value * k + f",
            "return doc['e'].size() == 0 ? -1 : doc['e'].value",
            "if (doc['l'].empty) { return 0 } return doc['l'].size() + doc['d'].value",
            "boolean big = doc['l'].value > 100; if (big) { return 1 } return 2",
            "if (doc['l'].value == params.n || doc['d'].value != 0.25) { return 1 } return doc['b'].value == true ? 2 : 3",
            "def x = doc['l'].value; float f = 7f; return x == f ? 1 : x == 7L ? 2 : 3",
            "def v = doc['d'].value; def w = v * v; return Math.sqrt(w) + params.o",
            "return doc['l'].size() == 0 ? 0 : doc['l'].value",
            "return doc['e'].size() == 0 ? 0 : doc['e'].value",
            "return doc['l'].isEmpty() ? params.f : doc['l'].value",
            "return doc['l'].value > 0 ? doc['d'].value : doc['l'].value",
            "double s = 0; for (int i = 0; i < 10; ++i) { s += i; } return s",
            "double s = 0; for (double v : doc['d']) { s += v } return s",
            "long s = 0; for (long v : doc['l']) { s += v * v } return s",
            "int s = 0; for (int v : doc['d']) { s += v } return s",
            "double s = 0; for (def v : doc['l']) { s += v } return s",
            "double s = 0; for (double v : doc['e']) { s += v } return s + 1",
            "int i = 0; double s = 0; while (i < 5) { s += doc['d'].value * i; i++ } return s",
            "for (int i = 0; i < 100; ++i) { if (i * i > doc['l'].value) { return i } } return -1",
            "double s = 0; for (int i = 0; i < 2000000; ++i) { s += 1 } return s",
            "double s = 0; for (int i = 0; i < 600; ++i) { for (int j = 0; j < 600; ++j) { s += 1 } } return s",
            "double s = 0; for (int i = 0; i < 2000; ++i) { for (int j = 0; j < 600; ++j) { s += 1 } } return s",
            "long p = 1; for (int i = 0; i < 70; ++i) { p *= 3 } return p",
            "int n = 0; for (int i = 10; i > 0; i -= 3) { n++ } return n",
            "double s = 0; for (boolean b : doc['b']) { s += b ? 1 : 0 } return s",
        ];
        for (ctx, body) in bodies.iter().map(|b| (Context::Score, *b)).chain(bodies.iter().map(|b| (Context::Filter, *b))) {
            let src = if ctx == Context::Filter { as_filter(body) } else { body.to_string() };
            let script = match engine.compile(&src, ctx) {
                Ok(s) => s,
                // filters have no `_score`
                Err(_) if ctx == Context::Filter && body.contains("_score") => continue,
                Err(e) => panic!("{src}: {e:?}"),
            };
            let (mut fast, mut slow) = (script.instance(), script.instance());
            fast.set_params(&params);
            slow.set_params(&params);
            slow.disable_fast_path();
            for d in &docs {
                let mut h1 = Doc { long: d.long, double: d.double };
                let mut h2 = Doc { long: d.long, double: d.double };
                let (a, b) = (fast.execute(&mut h1), slow.execute(&mut h2));
                assert!(same(&a, &b), "{src} on {}/{}: fast {a:?} vs interpreter {b:?}", d.long, d.double);
            }
            // a filter's `(c ? -1 : def) > 1` keeps per-branch boxing: the interpreter's
            // counting loops run in the interpreter (its typed registers beat closures there)
            let counting = body.contains("for (int") || body.contains("while");
            if (ctx == Context::Score || !body.contains('?')) && !counting {
                assert!(fast.fast_path_active(), "{src} should take the fast path");
            }
        }
    }
}
