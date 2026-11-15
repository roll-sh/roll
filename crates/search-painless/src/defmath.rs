//! Operators on `def` values (Elasticsearch's `DefMath` behind the `MIC` call site).
//!
//! The first lookup promotes the runtime classes of both operands — boxes unbox, `byte`,
//! `short` and `char` become `int`, anything else is `Object` — and picks the typed operator for
//! that type. Operands are then converted to it the way `MethodHandle.asType` does (a `Boolean`
//! passed where an `int` operator expects a number fails with `Cannot cast java.lang.Boolean to
//! java.lang.Integer`). A null operand selects the generic `Object` operator, which handles
//! string concatenation and fails with the `getClass()` NPE otherwise. Results are converted to
//! the call site type: explicitly in a cast context, to the left operand's class in compound
//! assignments, and only by widening otherwise.

use crate::ast::Op;
use crate::cast::{convert, prim_of, widens};
use crate::ir::{DefOp, Prim};
use crate::lookup::type_name;
use crate::types::Ty;
use crate::value::Value;
use crate::vm::{cce, npe, Throw, Thrown, VR};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum P {
    Int,
    Long,
    Float,
    Double,
    Bool,
    Obj,
}

fn promote1(v: &Value) -> P {
    match prim_of(v) {
        Some(Prim::Byte | Prim::Short | Prim::Char | Prim::Int) => P::Int,
        Some(Prim::Long) => P::Long,
        Some(Prim::Float) => P::Float,
        Some(Prim::Double) => P::Double,
        Some(Prim::Bool) => P::Bool,
        None => P::Obj,
    }
}

fn promote2(a: P, b: P) -> P {
    if a == P::Obj || b == P::Obj {
        return P::Obj;
    }
    if a == P::Bool && b == P::Bool {
        return P::Bool;
    }
    if a == P::Double || b == P::Double {
        P::Double
    } else if a == P::Float || b == P::Float {
        P::Float
    } else if a == P::Long || b == P::Long {
        P::Long
    } else {
        P::Int
    }
}

fn box_name(p: P) -> &'static str {
    match p {
        P::Int => "java.lang.Integer",
        P::Long => "java.lang.Long",
        P::Float => "java.lang.Float",
        P::Double => "java.lang.Double",
        P::Bool => "java.lang.Boolean",
        P::Obj => "java.lang.Object",
    }
}

fn prim(p: P) -> Prim {
    match p {
        P::Int => Prim::Int,
        P::Long => Prim::Long,
        P::Float => Prim::Float,
        P::Double => Prim::Double,
        P::Bool => Prim::Bool,
        P::Obj => Prim::Int,
    }
}

const CLASS_CAST_FRAME: &str = "java.base/java.lang.Class.cast(Class.java:3495)";

/// `asType` conversion of an operand to the typed operator's parameter.
fn arg(v: &Value, p: P) -> Result<Value, Thrown> {
    let from = prim_of(v).expect("operand is a box");
    let to = prim(p);
    if from == to || (p != P::Bool && from != Prim::Bool && widens(from, to)) {
        return Ok(convert(v, to));
    }
    Err(cce(format!("Cannot cast {} to {}", from.box_name(), box_name(p))).with_frames(&[CLASS_CAST_FRAME]))
}

fn canonical(v: &Value) -> String {
    type_name(v.runtime_ty())
}

fn types_error(op: &str, a: &Value, b: &Value) -> Thrown {
    // `DefMath.add` copies its operands into locals before calling `getClass()`
    let (l, r) = ("left", "right");
    if a.is_null() {
        return npe(format!("Cannot invoke \"Object.getClass()\" because \"{l}\" is null"));
    }
    if b.is_null() {
        return npe(format!("Cannot invoke \"Object.getClass()\" because \"{r}\" is null"));
    }
    cce(format!("Cannot apply [{op}] operation to types [{}] and [{}].", canonical(a), canonical(b)))
}

fn div_zero() -> Thrown {
    Throw::new(crate::types::cls::ARITHMETIC_EXCEPTION, Some("/ by zero".to_string()))
}

/// Number-ish view used by the generic `Object` operators.
enum Num {
    I(i32),
    L(i64),
    F(f32),
    D(f64),
    /// Any other `Number` (`doubleValue`, `longValue`): it takes its partner's width, else int.
    N(f64, i64),
}

fn num(v: &Value) -> Option<Num> {
    Some(match *v {
        Value::Byte(x) => Num::I(x as i32),
        Value::Short(x) => Num::I(x as i32),
        Value::Char(x) => Num::I(x as i32),
        Value::Int(x) => Num::I(x),
        Value::Long(x) => Num::L(x),
        Value::Float(x) => Num::F(x),
        Value::Double(x) => Num::D(x),
        _ => {
            let (d, l) = v.other_number()?;
            Num::N(d, l)
        }
    })
}

/// `doubleValue()` / `longValue()` of a numeric operand of the `Object` operators.
fn num_f64(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.other_number().map(|n| n.0))
}

fn num_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.other_number().map(|n| n.1))
}

fn typed(op: Op, p: P, a: &Value, b: &Value) -> VR {
    let sym = op.symbol();
    match p {
        P::Bool => {
            let (x, y) = (matches!(a, Value::Bool(true)), matches!(b, Value::Bool(true)));
            Ok(Value::Bool(match op {
                Op::BwAnd => x & y,
                Op::BwOr => x | y,
                Op::Xor => x ^ y,
                _ => return Err(cce(format!("Cannot apply [{sym}] operation to type [boolean]"))),
            }))
        }
        P::Int => {
            let (x, y) = (arg(a, p)?, arg(b, p)?);
            let (Value::Int(x), Value::Int(y)) = (x, y) else { unreachable!() };
            Ok(Value::Int(match op {
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return Err(div_zero());
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return Err(div_zero());
                    }
                    x.wrapping_rem(y)
                }
                Op::BwAnd => x & y,
                Op::BwOr => x | y,
                Op::Xor => x ^ y,
                _ => unreachable!(),
            }))
        }
        P::Long => {
            let (x, y) = (arg(a, p)?, arg(b, p)?);
            let (Value::Long(x), Value::Long(y)) = (x, y) else { unreachable!() };
            Ok(Value::Long(match op {
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return Err(div_zero());
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return Err(div_zero());
                    }
                    x.wrapping_rem(y)
                }
                Op::BwAnd => x & y,
                Op::BwOr => x | y,
                Op::Xor => x ^ y,
                _ => unreachable!(),
            }))
        }
        P::Float | P::Double => {
            if matches!(op, Op::BwAnd | Op::BwOr | Op::Xor) {
                // Elasticsearch names float for both widths here
                return Err(cce(format!("Cannot apply [{sym}] operation to type [float]")));
            }
            if p == P::Float {
                let (x, y) = (arg(a, p)?, arg(b, p)?);
                let (Value::Float(x), Value::Float(y)) = (x, y) else { unreachable!() };
                Ok(Value::Float(match op {
                    Op::Add => x + y,
                    Op::Sub => x - y,
                    Op::Mul => x * y,
                    Op::Div => x / y,
                    Op::Rem => x % y,
                    _ => unreachable!(),
                }))
            } else {
                let (x, y) = (arg(a, p)?, arg(b, p)?);
                let (Value::Double(x), Value::Double(y)) = (x, y) else { unreachable!() };
                Ok(Value::Double(match op {
                    Op::Add => x + y,
                    Op::Sub => x - y,
                    Op::Mul => x * y,
                    Op::Div => x / y,
                    Op::Rem => x % y,
                    _ => unreachable!(),
                }))
            }
        }
        P::Obj => generic(op, a, b),
    }
}

/// The `Object` operators (`DefMath.add(Object, Object)` and friends).
fn generic(op: Op, a: &Value, b: &Value) -> VR {
    let sym = op.symbol();
    if op == Op::Add {
        if let Value::Str(s) = a {
            let mut out = s.as_str().to_string();
            b.write_java_string(&mut out, 0);
            return Ok(Value::string(out));
        }
        if let Value::Str(s) = b {
            let mut out = a.to_java_string();
            out.push_str(s.as_str());
            return Ok(Value::string(out));
        }
    }
    match op {
        Op::BwAnd | Op::BwOr | Op::Xor => {
            if let (Value::Bool(x), Value::Bool(y)) = (a, b) {
                return Ok(Value::Bool(match op {
                    Op::BwAnd => x & y,
                    Op::BwOr => x | y,
                    _ => x ^ y,
                }));
            }
            if a.is_null() || b.is_null() {
                // `longIntegralValue(Object o)` / `intIntegralValue(Object o)` of the null side
                return Err(npe("Cannot invoke \"Object.getClass()\" because \"o\" is null"));
            }
            let long = matches!(a, Value::Long(_)) || matches!(b, Value::Long(_));
            if long {
                let (x, y) = (long_integral(a)?, long_integral(b)?);
                return Ok(Value::Long(match op {
                    Op::BwAnd => x & y,
                    Op::BwOr => x | y,
                    _ => x ^ y,
                }));
            }
            let (x, y) = (int_integral(a)?, int_integral(b)?);
            return Ok(Value::Int(match op {
                Op::BwAnd => x & y,
                Op::BwOr => x | y,
                _ => x ^ y,
            }));
        }
        _ => {}
    }
    let (Some(x), Some(y)) = (num(a), num(b)) else {
        return Err(types_error(sym, a, b));
    };
    let is = |n: &Num, k: u8| matches!((n, k), (Num::D(_), 3) | (Num::F(_), 2) | (Num::L(_), 1));
    let kind = if is(&x, 3) || is(&y, 3) {
        3
    } else if is(&x, 2) || is(&y, 2) {
        2
    } else if is(&x, 1) || is(&y, 1) {
        1
    } else {
        0
    };
    let f = |n: &Num| match *n {
        Num::I(v) => v as f64,
        Num::L(v) => v as f64,
        Num::F(v) => v as f64,
        Num::D(v) => v,
        Num::N(d, _) => d,
    };
    let l = |n: &Num| match *n {
        Num::I(v) => v as i64,
        Num::L(v) => v,
        Num::F(v) => v as i64,
        Num::D(v) => v as i64,
        Num::N(_, l) => l,
    };
    Ok(match kind {
        3 => {
            let (x, y) = (f(&x), f(&y));
            Value::Double(match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Rem => x % y,
                _ => return Err(types_error(sym, a, b)),
            })
        }
        2 => {
            let (x, y) = (f(&x) as f32, f(&y) as f32);
            Value::Float(match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Rem => x % y,
                _ => return Err(types_error(sym, a, b)),
            })
        }
        1 => {
            let (x, y) = (l(&x), l(&y));
            Value::Long(match op {
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return Err(div_zero());
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return Err(div_zero());
                    }
                    x.wrapping_rem(y)
                }
                _ => return Err(types_error(sym, a, b)),
            })
        }
        _ => {
            let (x, y) = (l(&x) as i32, l(&y) as i32);
            Value::Int(match op {
                Op::Add => x.wrapping_add(y),
                Op::Sub => x.wrapping_sub(y),
                Op::Mul => x.wrapping_mul(y),
                Op::Div => {
                    if y == 0 {
                        return Err(div_zero());
                    }
                    x.wrapping_div(y)
                }
                Op::Rem => {
                    if y == 0 {
                        return Err(div_zero());
                    }
                    x.wrapping_rem(y)
                }
                _ => return Err(types_error(sym, a, b)),
            })
        }
    })
}

fn long_integral(v: &Value) -> Result<i64, Thrown> {
    match v {
        Value::Long(x) => Ok(*x),
        Value::Int(_) | Value::Short(_) | Value::Byte(_) | Value::Char(_) => Ok(v.as_i64().unwrap()),
        _ => Err(cce(format!("Cannot convert [{}] to an integral value.", canonical(v)))),
    }
}

fn int_integral(v: &Value) -> Result<i32, Thrown> {
    match v {
        Value::Int(_) | Value::Short(_) | Value::Byte(_) | Value::Char(_) => Ok(v.as_i64().unwrap() as i32),
        _ => Err(cce(format!("Cannot convert [{}] to an integral value.", canonical(v)))),
    }
}

fn shift(op: Op, a: &Value, b: &Value) -> VR {
    // shifts dispatch on the left operand only; the distance is converted to long
    let p = promote1(a);
    let sym = op.symbol();
    let dist = |b: &Value| -> Result<i64, Thrown> {
        match prim_of(b) {
            Some(f) if f != Prim::Bool && widens(f, Prim::Long) => Ok(b.as_i64().unwrap()),
            Some(f) => Err(cce(format!("Cannot cast {} to java.lang.Long", f.box_name())).with_frames(&[CLASS_CAST_FRAME])),
            None => {
                if b.is_null() {
                    Err(npe("Cannot invoke \"java.lang.Number.longValue()\" because the return value of \"sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)\" is null"))
                } else {
                    Err(cce(format!("Cannot cast {} to java.lang.Long", crate::lookup::java_binary_name(b.class()))).with_frames(&[CLASS_CAST_FRAME]))
                }
            }
        }
    };
    match p {
        P::Int => {
            let x = arg(a, P::Int)?;
            let Value::Int(x) = x else { unreachable!() };
            let s = dist(b)? as u32;
            Ok(Value::Int(match op {
                Op::Lsh => x.wrapping_shl(s),
                Op::Rsh => x.wrapping_shr(s),
                _ => ((x as u32).wrapping_shr(s)) as i32,
            }))
        }
        P::Long => {
            let Value::Long(x) = arg(a, P::Long)? else { unreachable!() };
            let s = dist(b)? as u32;
            Ok(Value::Long(match op {
                Op::Lsh => x.wrapping_shl(s),
                Op::Rsh => x.wrapping_shr(s),
                _ => ((x as u64).wrapping_shr(s)) as i64,
            }))
        }
        P::Float => Err(cce(format!("Cannot apply [{sym}] operation to type [float]"))),
        P::Double => Err(cce(format!("Cannot apply [{sym}] operation to type [double]"))),
        P::Bool => Err(cce(format!("Cannot apply [{sym}] operation to type [boolean]"))),
        P::Obj => {
            // the generic shift reads the value through `intIntegralValue(Object o)`
            if a.is_null() {
                return Err(npe("Cannot invoke \"Object.getClass()\" because \"o\" is null"));
            }
            if b.is_null() {
                return Err(npe("Cannot invoke \"Object.getClass()\" because \"right\" is null"));
            }
            Err(cce(format!("Cannot apply [{sym}] operation to types [{}] and [{}].", canonical(a), canonical(b))))
        }
    }
}

/// A binary operator on `def` operands.
/// [`binary`] for the overwhelmingly common case: `+ - * /` on `int`, `long` and `double`
/// operands whose result needs no conversion. `None` means "take the general path" (which also
/// raises every error).
#[inline(always)]
pub fn fast_binary(op: Op, a: &Value, b: &Value, info: &DefOp) -> Option<Value> {
    let r = match (a, b) {
        (&Value::Double(x), &Value::Double(y)) => double_op(op, x, y)?,
        (&Value::Double(x), &Value::Int(y)) => double_op(op, x, y as f64)?,
        (&Value::Double(x), &Value::Long(y)) => double_op(op, x, y as f64)?,
        (&Value::Int(x), &Value::Double(y)) => double_op(op, x as f64, y)?,
        (&Value::Long(x), &Value::Double(y)) => double_op(op, x as f64, y)?,
        (&Value::Long(x), &Value::Long(y)) => Value::Long(long_op(op, x, y)?),
        (&Value::Long(x), &Value::Int(y)) => Value::Long(long_op(op, x, y as i64)?),
        (&Value::Int(x), &Value::Long(y)) => Value::Long(long_op(op, x as i64, y)?),
        (&Value::Int(x), &Value::Int(y)) => Value::Int(match op {
            Op::Add => x.wrapping_add(y),
            Op::Sub => x.wrapping_sub(y),
            Op::Mul => x.wrapping_mul(y),
            Op::Div if y != 0 => x.wrapping_div(y),
            _ => return None,
        }),
        _ => return None,
    };
    if info.compound {
        // the cast back to the receiver's class (`dynamicReceiverCast`) is the identity when
        // the result already has it
        return (std::mem::discriminant(&r) == std::mem::discriminant(a)).then_some(r);
    }
    if info.ret == Ty::DEF {
        return Some(r);
    }
    // the call-site conversion `finish` would apply (`cast_math_result`): same type, or a
    // widening one, which is legal implicitly and explicitly alike
    match (&r, info.ret) {
        (Value::Double(_), Ty::DOUBLE) | (Value::Long(_), Ty::LONG) | (Value::Int(_), Ty::INT) => Some(r),
        (&Value::Long(x), Ty::DOUBLE) => Some(Value::Double(x as f64)),
        (&Value::Int(x), Ty::DOUBLE) => Some(Value::Double(x as f64)),
        (&Value::Int(x), Ty::LONG) => Some(Value::Long(x as i64)),
        _ => None,
    }
}

#[inline(always)]
fn double_op(op: Op, x: f64, y: f64) -> Option<Value> {
    Some(Value::Double(match op {
        Op::Add => x + y,
        Op::Sub => x - y,
        Op::Mul => x * y,
        Op::Div => x / y,
        _ => return None,
    }))
}

#[inline(always)]
fn long_op(op: Op, x: i64, y: i64) -> Option<i64> {
    Some(match op {
        Op::Add => x.wrapping_add(y),
        Op::Sub => x.wrapping_sub(y),
        Op::Mul => x.wrapping_mul(y),
        Op::Div if y != 0 => x.wrapping_div(y),
        _ => return None,
    })
}

pub fn binary(op: Op, a: &Value, b: &Value, info: &DefOp) -> VR {
    let r = if matches!(op, Op::Lsh | Op::Rsh | Op::Ush) {
        if !a.is_null() && !b.is_null() {
            let p = promote1(a);
            link_check(info, &[p_name(p), "long"], p, 2)?;
        }
        shift(op, a, b)?
    } else if a.is_null() || b.is_null() {
        generic(op, a, b)?
    } else {
        let p = promote2(promote1(a), promote1(b));
        link_check(info, &[p_name(p), p_name(p)], p, 2)?;
        typed(op, p, a, b)?
    };
    finish(r, a, info)
}

fn p_name(p: P) -> &'static str {
    match p {
        P::Int => "int",
        P::Long => "long",
        P::Float => "float",
        P::Double => "double",
        P::Bool => "boolean",
        P::Obj => "Object",
    }
}

/// Linking the operator call site (`DefBootstrap.MIC`): the typed `DefMath` handle for the
/// promoted operand types must convert to the call site's type. A primitive result that cannot
/// (`int y = longDef + 1`) fails before the operator runs, with `MethodHandle.asType`'s
/// `WrongMethodTypeException` as the cause; explicit casts convert between any non-boolean
/// primitives.
fn link_check(info: &DefOp, params: &[&str], ret: P, arity: usize) -> Result<(), Thrown> {
    let to = info.ret;
    if info.compound || ret == P::Obj || to == Ty::DEF || to == Ty::OBJECT {
        return Ok(());
    }
    let from = match ret {
        P::Int => Prim::Int,
        P::Long => Prim::Long,
        P::Float => Prim::Float,
        P::Double => Prim::Double,
        P::Bool => Prim::Bool,
        P::Obj => return Ok(()),
    };
    let Some(tp) = Prim::of(to) else { return Ok(()) };
    if to.is_primitive() {
        if from == tp {
            return Ok(());
        }
        if info.explicit {
            if from == Prim::Bool || tp == Prim::Bool {
                return Err(cce(format!("Cannot cast {} to {}", from.java_name(), tp.java_name())));
            }
            return Ok(());
        }
        if from != Prim::Bool && tp != Prim::Bool && crate::cast::widens(from, tp) {
            return Ok(());
        }
    } else if from == tp || info.explicit {
        // boxing to the result's own wrapper; explicit conversions are checked when converting
        return Ok(());
    }
    let site_name = |t: Ty| -> String {
        if t == Ty::DEF || t == Ty::OBJECT {
            "Object".to_string()
        } else if t.is_primitive() {
            Prim::of(t).map(|p| p.java_name().to_string()).unwrap_or_default()
        } else {
            let n = crate::lookup::type_name(t);
            n.rsplit('.').next().unwrap_or(&n).to_string()
        }
    };
    let site: Vec<String> = info.site[..arity].iter().map(|t| site_name(*t)).collect();
    let cause = Throw::new(
        crate::types::cls::WRONG_METHOD_TYPE_EXCEPTION,
        Some(format!("cannot convert MethodHandle({}){} to ({}){}", params.join(","), p_name(ret), site.join(","), site_name(to))),
    );
    let msg = if to.is_primitive() {
        format!("Cannot cast from: {} to {}", from.java_name(), tp.java_name())
    } else {
        format!("Cannot cast from: {} to class {}", from.java_name(), tp.box_name())
    };
    Err(cce(msg).with_cause(&cause))
}

fn finish(r: Value, left: &Value, info: &DefOp) -> VR {
    if info.compound {
        // cast back to the receiver's class (`DefMath.dynamicReceiverCast`)
        if left.is_null() || r.is_null() {
            return Ok(r);
        }
        return match (prim_of(left), prim_of(&r)) {
            (Some(lp), Some(rp)) if lp != rp && lp != Prim::Bool && rp != Prim::Bool => Ok(convert(&r, lp)),
            (Some(lp), Some(rp)) if lp != rp => Err(cce(format!("Cannot cast {} to {}", rp.java_name(), lp.java_name()))),
            _ => Ok(r),
        };
    }
    if info.ret == Ty::DEF {
        return Ok(r);
    }
    crate::cast::cast_math_result(r, info.ret, info.explicit)
}

/// A unary operator on a `def` operand (`-x`, `~x`, `+x`).
pub fn unary(op: Op, a: &Value, info: &DefOp) -> VR {
    let p = promote1(a);
    // `!` is not a `DefMath` operator on def (it casts to boolean first)
    if matches!(op, Op::Sub | Op::Add | Op::BwNot) {
        link_check(info, &[p_name(p)], p, 1)?;
    }
    let r = match (op, p) {
        (Op::Sub, P::Int) => {
            let Value::Int(x) = arg(a, P::Int)? else { unreachable!() };
            Value::Int(x.wrapping_neg())
        }
        (Op::Sub, P::Long) => Value::Long(a.as_i64().unwrap().wrapping_neg()),
        (Op::Sub, P::Float) => match a {
            Value::Float(f) => Value::Float(-f),
            _ => unreachable!(),
        },
        (Op::Sub, P::Double) => Value::Double(-a.as_f64().unwrap()),
        (Op::Add, P::Int) => arg(a, P::Int)?,
        (Op::Add, P::Long | P::Float | P::Double) => a.clone(),
        (Op::BwNot, P::Int) => {
            let Value::Int(x) = arg(a, P::Int)? else { unreachable!() };
            Value::Int(!x)
        }
        (Op::BwNot, P::Long) => Value::Long(!a.as_i64().unwrap()),
        (Op::BwNot, P::Float) => return Err(cce("Cannot apply not [~] to type [float]")),
        (Op::BwNot, P::Double) => return Err(cce("Cannot apply not [~] to type [double]")),
        (Op::BwNot, P::Bool) => return Err(cce("Cannot apply not [~] to type [boolean]")),
        (_, P::Bool) => return Err(cce(format!("Cannot apply [{}] operation to type [boolean]", op.symbol()))),
        (_, P::Obj) => {
            if a.is_null() {
                return Err(npe("Cannot invoke \"Object.getClass()\" because \"unary\" is null"));
            }
            let sym = if op == Op::BwNot { "~" } else { op.symbol() };
            return Err(cce(format!("Cannot apply [{sym}] operation to type [{}].", canonical(a))));
        }
        _ => return Err(cce("Cannot apply operator")),
    };
    finish(r, a, info)
}

/// Comparison operators on `def` operands.
/// [`compare`] for ordering operators on `int`, `long` and `double` operands (promoted the way
/// `DefMath` promotes them); `None` takes the general path.
#[inline(always)]
pub fn fast_compare(op: Op, a: &Value, b: &Value) -> Option<bool> {
    #[inline(always)]
    fn ord<T: PartialOrd>(op: Op, x: T, y: T) -> Option<bool> {
        Some(match op {
            Op::Lt => x < y,
            Op::Lte => x <= y,
            Op::Gt => x > y,
            Op::Gte => x >= y,
            _ => return None,
        })
    }
    match (a, b) {
        (&Value::Int(x), &Value::Int(y)) => ord(op, x, y),
        (&Value::Long(x), &Value::Long(y)) => ord(op, x, y),
        (&Value::Long(x), &Value::Int(y)) => ord(op, x, y as i64),
        (&Value::Int(x), &Value::Long(y)) => ord(op, x as i64, y),
        (&Value::Double(x), &Value::Double(y)) => ord(op, x, y),
        (&Value::Double(x), &Value::Int(y)) => ord(op, x, y as f64),
        (&Value::Double(x), &Value::Long(y)) => ord(op, x, y as f64),
        (&Value::Int(x), &Value::Double(y)) => ord(op, x as f64, y),
        (&Value::Long(x), &Value::Double(y)) => ord(op, x as f64, y),
        _ => None,
    }
}

pub fn compare(op: Op, a: &Value, b: &Value) -> Result<bool, Thrown> {
    match op {
        Op::Eq | Op::Ne => {
            let eq = def_eq(a, b)?;
            Ok(if op == Op::Eq { eq } else { !eq })
        }
        Op::Eqr => Ok(crate::vm::same_ref_pub(a, b)),
        Op::Ner => Ok(!crate::vm::same_ref_pub(a, b)),
        _ => {
            let sym = op.symbol();
            if a.is_null() || b.is_null() {
                return Err(types_error(sym, a, b));
            }
            let p = promote2(promote1(a), promote1(b));
            let ord = match p {
                P::Bool => return Err(cce(format!("Cannot apply [{sym}] operation to type [boolean]"))),
                P::Obj => {
                    let (Some(_), Some(_)) = (num(a), num(b)) else {
                        return Err(types_error(sym, a, b));
                    };
                    cmp_values(a, b)
                }
                P::Int => {
                    let (Value::Int(x), Value::Int(y)) = (arg(a, p)?, arg(b, p)?) else { unreachable!() };
                    x.partial_cmp(&y)
                }
                P::Long => {
                    let (Value::Long(x), Value::Long(y)) = (arg(a, p)?, arg(b, p)?) else { unreachable!() };
                    x.partial_cmp(&y)
                }
                P::Float => {
                    let (Value::Float(x), Value::Float(y)) = (arg(a, p)?, arg(b, p)?) else { unreachable!() };
                    x.partial_cmp(&y)
                }
                P::Double => {
                    let (Value::Double(x), Value::Double(y)) = (arg(a, p)?, arg(b, p)?) else { unreachable!() };
                    x.partial_cmp(&y)
                }
            };
            use std::cmp::Ordering::*;
            Ok(match (op, ord) {
                (_, None) => false,
                (Op::Lt, Some(o)) => o == Less,
                (Op::Lte, Some(o)) => o != Greater,
                (Op::Gt, Some(o)) => o == Greater,
                (Op::Gte, Some(o)) => o != Less,
                _ => false,
            })
        }
    }
}

fn cmp_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    let float = matches!(a, Value::Double(_) | Value::Float(_)) || matches!(b, Value::Double(_) | Value::Float(_));
    let long = matches!(a, Value::Long(_)) || matches!(b, Value::Long(_));
    if float {
        num_f64(a)?.partial_cmp(&num_f64(b)?)
    } else if long {
        num_i64(a)?.partial_cmp(&num_i64(b)?)
    } else {
        // `intValue()` of both
        (num_i64(a)? as i32).partial_cmp(&(num_i64(b)? as i32))
    }
}

/// `==` on `def` operands: numeric across box types, `equals` otherwise, nulls allowed.
pub fn def_eq(a: &Value, b: &Value) -> Result<bool, Thrown> {
    if a.is_null() || b.is_null() {
        return Ok(a.is_null() && b.is_null());
    }
    let (pa, pb) = (promote1(a), promote1(b));
    let p = promote2(pa, pb);
    match p {
        P::Bool => Ok(a.java_equals(b)),
        P::Obj => Ok(object_eq(a, b)),
        P::Int | P::Long | P::Float | P::Double => {
            if pa == P::Bool || pb == P::Bool {
                // a Boolean where a number is expected fails the operand conversion
                let bad = if pa == P::Bool { a } else { b };
                let _ = bad;
                return Err(cce(format!("Cannot cast java.lang.Boolean to {}", box_name(p))).with_frames(&[CLASS_CAST_FRAME]));
            }
            Ok(match p {
                P::Int | P::Long => a.as_i64() == b.as_i64(),
                P::Float => float_value(a) == float_value(b),
                _ => a.as_f64() == b.as_f64(),
            })
        }
    }
}

/// `Number.floatValue()`: a long converts in one rounding, not through double.
fn float_value(v: &Value) -> Option<f32> {
    match *v {
        Value::Long(l) => Some(l as f32),
        _ => v.as_f64().map(|x| x as f32),
    }
}

/// `DefMath.eq(Object, Object)`.
fn object_eq(a: &Value, b: &Value) -> bool {
    if a.class() == b.class() {
        return a.java_equals(b);
    }
    if let (Some(x), Some(y)) = (num(a), num(b)) {
        let _ = (x, y);
        let fl = |v: &Value| matches!(v, Value::Double(_) | Value::Float(_));
        if fl(a) || fl(b) {
            return num_f64(a) == num_f64(b);
        }
        if matches!(a, Value::Long(_)) || matches!(b, Value::Long(_)) {
            return num_i64(a) == num_i64(b);
        }
        return num_i64(a).map(|x| x as i32) == num_i64(b).map(|x| x as i32);
    }
    a.java_equals(b)
}

// ------------------------------------------------------------------------------ call-site linking

// Painless links each `def` operator call site (`DefBootstrap.MIC`) on its first execution:
// the operator for the operands' classes behind a guard testing those classes. Later
// executions run the guard first — and a guard reading the class of a null operand fails with
// its own parameter name (`leftObject` / `rightObject`) instead of the operator's (`left`) — and
// a guard miss relinks the site to the generic operator for good. The state lives in the
// program, shared by every execution like the compiled class's call sites.

const UNLINKED: u64 = 0;
const GENERIC: u64 = 1;
const GUARDED: u64 = 2;

fn class_code(v: &Value) -> u64 {
    if v.is_null() { 0 } else { v.class().0 as u64 + 1 }
}

fn guarded(a: &Value, b: &Value) -> u64 {
    GUARDED | class_code(a) << 8 | class_code(b) << 32
}

/// Which operands the guard of a site tests: the left only when the right parameter is typed,
/// the right only when the left one is, both otherwise.
fn guard_sides(info: &DefOp) -> (bool, bool) {
    use crate::ir::SiteArg;
    if info.args[1] != SiteArg::Object {
        (true, false)
    } else if info.args[0] != SiteArg::Object {
        (false, true)
    } else {
        (true, true)
    }
}

/// Operators whose sites take part: arithmetic and bitwise binaries and the ordering
/// comparisons (equality allows nulls).
fn links(op: Op) -> bool {
    !matches!(op, Op::Eq | Op::Ne | Op::Eqr | Op::Ner)
}

/// The linking of a unary operator site (shifts link like unaries, on the shifted value):
/// the first execution looks the operator up by the operand's class — a null operand fails
/// there, leaving the site unlinked — and later ones run the class guard.
pub fn link_unary(prog: &crate::vm::Program, info: u32, a: &Value) -> Result<(), Thrown> {
    use std::sync::atomic::Ordering::Relaxed;
    let site = &prog.def_links[info as usize];
    let state = site.load(Relaxed);
    let null = |name: &str| npe(format!("Cannot invoke \"Object.getClass()\" because \"{name}\" is null"));
    match state {
        UNLINKED if prog.fast_ran.load(Relaxed) && a.is_null() => Err(null("leftObject")),
        UNLINKED if a.is_null() => Err(null("args[0]")),
        UNLINKED => {
            site.store(guarded(a, &Value::Null), Relaxed);
            Ok(())
        }
        GENERIC => Ok(()),
        _ => {
            if a.is_null() {
                return Err(null("leftObject"));
            }
            if class_code(a) != (state >> 8) & 0xFF_FFFF {
                site.store(GENERIC, Relaxed);
            }
            Ok(())
        }
    }
}

/// `+` without a primitive parameter may be string concatenation: its guard lets nulls through
/// to the generic operator (`OPERATOR_ALLOWS_NULL`).
fn allows_null(op: Op, info: &DefOp) -> bool {
    op == Op::Add && !info.args.contains(&crate::ir::SiteArg::Primitive)
}

impl crate::vm::Program {
    /// Whether an execution of `def` operator site `info` on these operands leaves its linking
    /// unchanged (so the fast path may run it).
    #[inline]
    pub fn linked(&self, info: u32, a: &Value, b: &Value) -> bool {
        let state = self.def_links[info as usize].load(std::sync::atomic::Ordering::Relaxed);
        if state == GENERIC {
            return true;
        }
        if state == UNLINKED {
            return false;
        }
        let def = &self.defops[info as usize];
        let (l, r) = guard_sides(def);
        let want = guarded(a, b);
        (!l || (state >> 8) & 0xFF_FFFF == (want >> 8) & 0xFF_FFFF) && (!r || state >> 32 == want >> 32)
    }
}

/// Runs the linking step of `def` operator site `info` before the operator: links an unlinked
/// site, checks a guarded one (failing like its guard on a null operand) and relinks it to the
/// generic operator on a miss.
pub fn link(prog: &crate::vm::Program, info: u32, op: Op, a: &Value, b: &Value) -> Result<(), Thrown> {
    use std::sync::atomic::Ordering::Relaxed;
    if !links(op) {
        // nothing to track: let the fast path take the site from now on
        prog.def_links[info as usize].store(GENERIC, Relaxed);
        return Ok(());
    }
    if matches!(op, Op::Lsh | Op::Rsh | Op::Ush) {
        return link_unary(prog, info, a);
    }
    let site = &prog.def_links[info as usize];
    let def = &prog.defops[info as usize];
    let state = site.load(Relaxed);
    match state {
        UNLINKED if prog.fast_ran.load(Relaxed) => {
            // linked by fast-path runs to classes not recorded: only its null checks are known
            let (l, r) = guard_sides(def);
            if !allows_null(op, def) {
                if l && a.is_null() {
                    return Err(npe("Cannot invoke \"Object.getClass()\" because \"leftObject\" is null"));
                }
                if r && b.is_null() {
                    return Err(npe("Cannot invoke \"Object.getClass()\" because \"rightObject\" is null"));
                }
            }
            site.store(guarded(a, b), Relaxed);
        }
        UNLINKED => site.store(guarded(a, b), Relaxed),
        GENERIC => {}
        _ => {
            let (l, r) = guard_sides(def);
            if allows_null(op, def) && ((l && a.is_null()) || (r && b.is_null())) {
                return Ok(());
            }
            let guard_npe = |name: &str| npe(format!("Cannot invoke \"Object.getClass()\" because \"{name}\" is null"));
            let mut hit = true;
            if l {
                if a.is_null() {
                    return Err(guard_npe("leftObject"));
                }
                hit = class_code(a) == (state >> 8) & 0xFF_FFFF;
            }
            if r && hit {
                if b.is_null() {
                    return Err(guard_npe("rightObject"));
                }
                hit = class_code(b) == state >> 32;
            }
            if !hit {
                site.store(GENERIC, Relaxed);
            }
        }
    }
    Ok(())
}
