//! Runtime conversions ([`Cast`]) with the exact failure messages Elasticsearch produces.
//!
//! There are several distinct conversion paths in Elasticsearch and each fails differently:
//! `def` variables use `Def.defToX` (`cannot implicitly cast def [long] to int`), results of
//! `def` calls and field reads use `MethodHandle.asType` (`Cannot cast java.lang.Long to
//! java.lang.Integer`, with a `Class.cast` frame), results of `def` operators use the operator
//! call site (`Cannot cast from: long to int`), and reference downcasts are JVM `checkcast`s.

use crate::ir::{Cast, Prim};
use crate::java;
use crate::lookup::{java_binary_name, type_name};
use crate::types::Ty;
use crate::value::{Obj, Value};
use crate::vm::{cce, npe, Thrown, Vm, VR};

pub fn cast(vm: &mut Vm, v: Value, c: &Cast) -> VR {
    match c {
        Cast::Num { to, unbox } => {
            if v.is_null() {
                if let Some(from) = unbox {
                    return Err(unbox_null(*from, None));
                }
                return Ok(v);
            }
            Ok(convert(&v, *to))
        }
        Cast::Unbox(p) => {
            if v.is_null() {
                return Err(unbox_null(*p, None));
            }
            Ok(v)
        }
        Cast::DefTo { to, explicit, boxed } => def_to(&v, *to, *explicit, *boxed),
        Cast::DefToString { explicit } => match &v {
            Value::Null | Value::Str(_) => Ok(v),
            Value::Char(c) if *explicit => Ok(Value::string(java::char_to_string(*c))),
            _ => Err(def_cast_exception(&v, "java.lang.String", Some(!*explicit))),
        },
        Cast::AsType(t) => as_type(vm, v, *t),
        Cast::MathResult { to, explicit } => cast_math_result(v, *to, *explicit),
        Cast::StringToChar => match &v {
            Value::Null => Err(cce("cannot cast null java.lang.String to char")),
            Value::Str(s) => {
                if s.len16() != 1 {
                    return Err(cce("cannot cast java.lang.String with length not equal to one to char"));
                }
                Ok(Value::Char(s.char_at(0).unwrap()))
            }
            other => Ok(other.clone()),
        },
        Cast::CharToString => match &v {
            Value::Char(c) => Ok(Value::string(java::char_to_string(*c))),
            other => Ok(other.clone()),
        },
        Cast::Check(t) => check(vm, v, *t),
        Cast::Noop | Cast::Transparent => Ok(v),
    }
}

/// Unboxing a null box: the JVM's helpful message for the `xValue()` call Painless emits
/// (`Number.intValue()` for every numeric box, `Boolean.booleanValue()`,
/// `Character.charValue()`), naming the null expression when the compiler could describe it.
pub fn unbox_null(from: Prim, desc: Option<&str>) -> Thrown {
    let m = match from {
        Prim::Bool => "java.lang.Boolean.booleanValue()",
        Prim::Char => "java.lang.Character.charValue()",
        Prim::Byte => "java.lang.Number.byteValue()",
        Prim::Short => "java.lang.Number.shortValue()",
        Prim::Int => "java.lang.Number.intValue()",
        Prim::Long => "java.lang.Number.longValue()",
        Prim::Float => "java.lang.Number.floatValue()",
        Prim::Double => "java.lang.Number.doubleValue()",
    };
    let why = desc.map(|d| format!(" because {d} is null")).unwrap_or_default();
    npe(format!("Cannot invoke \"{m}\"{why}"))
}

/// The common numeric conversions, without the error paths: `None` sends the interpreter to
/// [`cast`]. Results are exactly what `cast` returns for these inputs.
#[inline(always)]
pub fn fast_cast(v: &Value, c: &Cast) -> Option<Value> {
    match (c, v) {
        (Cast::Num { to: Prim::Double, .. } | Cast::DefTo { to: Prim::Double, boxed: false, .. }, &Value::Double(x)) => Some(Value::Double(x)),
        (Cast::Num { to: Prim::Double, .. } | Cast::DefTo { to: Prim::Double, boxed: false, .. }, &Value::Int(x)) => Some(Value::Double(x as f64)),
        (Cast::Num { to: Prim::Double, .. } | Cast::DefTo { to: Prim::Double, boxed: false, .. }, &Value::Long(x)) => Some(Value::Double(x as f64)),
        (Cast::Num { to: Prim::Long, .. } | Cast::DefTo { to: Prim::Long, boxed: false, .. }, &Value::Long(x)) => Some(Value::Long(x)),
        (Cast::Num { to: Prim::Long, .. } | Cast::DefTo { to: Prim::Long, boxed: false, .. }, &Value::Int(x)) => Some(Value::Long(x as i64)),
        (Cast::Num { to: Prim::Int, .. } | Cast::DefTo { to: Prim::Int, boxed: false, .. }, &Value::Int(x)) => Some(Value::Int(x)),
        (Cast::MathResult { to: Ty::DOUBLE, .. }, &Value::Double(x)) => Some(Value::Double(x)),
        (Cast::MathResult { to: Ty::LONG, .. }, &Value::Long(x)) => Some(Value::Long(x)),
        (Cast::MathResult { to: Ty::INT, .. }, &Value::Int(x)) => Some(Value::Int(x)),
        (Cast::Unbox(_), &Value::Double(x)) => Some(Value::Double(x)),
        (Cast::Unbox(_), &Value::Long(x)) => Some(Value::Long(x)),
        (Cast::Unbox(_), &Value::Int(x)) => Some(Value::Int(x)),
        // `MethodHandle.asType` to a primitive: widening of a wrapper (what `as_type` does)
        (Cast::AsType(Ty::DOUBLE), &Value::Double(x)) => Some(Value::Double(x)),
        (Cast::AsType(Ty::DOUBLE), &Value::Long(x)) => Some(Value::Double(x as f64)),
        (Cast::AsType(Ty::DOUBLE), &Value::Int(x)) => Some(Value::Double(x as f64)),
        (Cast::AsType(Ty::LONG), &Value::Long(x)) => Some(Value::Long(x)),
        (Cast::AsType(Ty::LONG), &Value::Int(x)) => Some(Value::Long(x as i64)),
        (Cast::AsType(Ty::INT), &Value::Int(x)) => Some(Value::Int(x)),
        (Cast::AsType(Ty::BOOLEAN), &Value::Bool(x)) => Some(Value::Bool(x)),
        (Cast::Noop | Cast::Transparent, v) => Some(v.clone()),
        _ => None,
    }
}

/// Converts a number or char (primitive or boxed) to a primitive kind with Java semantics.
pub fn convert(v: &Value, to: Prim) -> Value {
    match to {
        Prim::Bool => v.clone(),
        Prim::Double => Value::Double(v.as_f64().unwrap_or(0.0)),
        Prim::Float => Value::Float(match *v {
            Value::Double(d) => d as f32,
            Value::Long(l) => l as f32,
            _ => v.as_f64().unwrap_or(0.0) as f32,
        }),
        Prim::Long => Value::Long(match *v {
            Value::Double(d) => java::d2l(d),
            Value::Float(f) => java::d2l(f as f64),
            _ => v.as_i64().unwrap_or(0),
        }),
        _ => {
            let i: i32 = match *v {
                Value::Double(d) => java::d2i(d),
                Value::Float(f) => java::d2i(f as f64),
                Value::Long(l) => l as i32,
                _ => v.as_i64().unwrap_or(0) as i32,
            };
            match to {
                Prim::Byte => Value::Byte(i as i8),
                Prim::Short => Value::Short(i as i16),
                Prim::Char => Value::Char(i as u16),
                _ => Value::Int(i),
            }
        }
    }
}

/// Primitive kind of a boxed value (`Integer` → int).
pub fn prim_of(v: &Value) -> Option<Prim> {
    Some(match v {
        Value::Bool(_) => Prim::Bool,
        Value::Byte(_) => Prim::Byte,
        Value::Short(_) => Prim::Short,
        Value::Char(_) => Prim::Char,
        Value::Int(_) => Prim::Int,
        Value::Long(_) => Prim::Long,
        Value::Float(_) => Prim::Float,
        Value::Double(_) => Prim::Double,
        _ => return None,
    })
}

/// `typeToUnboxedType(value.getClass()).getCanonicalName()`.
pub fn unboxed_name(v: &Value) -> String {
    match prim_of(v) {
        Some(p) => p.java_name().to_string(),
        None => type_name(v.runtime_ty()),
    }
}

/// `Def.castException`: `cannot [implicitly |explicitly ]cast def [long] to int`.
fn def_cast_exception(v: &Value, target: &str, implicit: Option<bool>) -> Thrown {
    let how = match implicit {
        None => "",
        Some(true) => "implicitly ",
        Some(false) => "explicitly ",
    };
    cce(format!("cannot {how}cast def [{}] to {target}", unboxed_name(v)))
}

fn get_class_npe() -> Thrown {
    npe("Cannot invoke \"Object.getClass()\" because \"value\" is null")
}

/// `Def.defToX{Implicit,Explicit}` and the boxed variants.
fn def_to(v: &Value, to: Prim, explicit: bool, boxed: bool) -> VR {
    if v.is_null() {
        return if boxed { Ok(Value::Null) } else { Err(get_class_npe()) };
    }
    let target = if boxed { to.box_name() } else { to.java_name() };
    // the boxed converters always report "explicitly"; defToboolean reports neither, and
    // defTodoubleExplicit names byte (both quirks of Elasticsearch)
    let how = if to == Prim::Bool {
        if boxed {
            Some(false)
        } else {
            None
        }
    } else if boxed {
        Some(false)
    } else {
        Some(!explicit)
    };
    let fail_target = if !boxed && explicit && to == Prim::Double { "byte" } else { target };
    let fail = || Err(def_cast_exception(v, fail_target, how));
    let from = match prim_of(v) {
        Some(p) => p,
        None => {
            if to == Prim::Char && explicit
                && let Value::Str(s) = v {
                    if s.len16() != 1 {
                        return Err(cce("cannot cast java.lang.String with length not equal to one to char"));
                    }
                    return Ok(Value::Char(s.char_at(0).unwrap()));
                }
            return fail();
        }
    };
    if to == Prim::Bool {
        return if from == Prim::Bool { Ok(v.clone()) } else { fail() };
    }
    if from == Prim::Bool {
        return fail();
    }
    if explicit {
        return Ok(convert(v, to));
    }
    let ok = match to {
        Prim::Byte => from == Prim::Byte,
        Prim::Short => matches!(from, Prim::Byte | Prim::Short),
        Prim::Char => from == Prim::Char,
        Prim::Int => matches!(from, Prim::Byte | Prim::Short | Prim::Char | Prim::Int),
        Prim::Long => matches!(from, Prim::Byte | Prim::Short | Prim::Char | Prim::Int | Prim::Long),
        Prim::Float => !matches!(from, Prim::Double),
        Prim::Double => true,
        Prim::Bool => false,
    };
    if ok {
        Ok(convert(v, to))
    } else {
        fail()
    }
}

fn rank(p: Prim) -> u8 {
    match p {
        Prim::Byte => 0,
        Prim::Short => 1,
        Prim::Char => 1,
        Prim::Int => 2,
        Prim::Long => 3,
        Prim::Float => 4,
        Prim::Double => 5,
        Prim::Bool => 9,
    }
}

/// Java widening primitive conversion check.
pub fn widens(from: Prim, to: Prim) -> bool {
    if from == to {
        return true;
    }
    if from == Prim::Bool || to == Prim::Bool || to == Prim::Char {
        return false;
    }
    if from == Prim::Char {
        return matches!(to, Prim::Int | Prim::Long | Prim::Float | Prim::Double);
    }
    if from == Prim::Short && to == Prim::Byte {
        return false;
    }
    rank(to) > rank(from)
}

const CLASS_CAST_FRAME: &str = "java.base/java.lang.Class.cast(Class.java:3495)";

/// `MethodHandle.asType(Object → t)` applied to the result of a dynamic call, field or index.
fn as_type(vm: &mut Vm, v: Value, t: Ty) -> VR {
    if let Some(to) = Prim::of(t).filter(|_| t.is_primitive()) {
        let Some(from) = prim_of(&v) else {
            if v.is_null() {
                // `ValueConversions.primitiveConversion` reads booleans and chars through `intValue`
                let m = match to {
                    Prim::Bool | Prim::Char => "java.lang.Number.intValue()",
                    Prim::Byte => "java.lang.Number.byteValue()",
                    Prim::Short => "java.lang.Number.shortValue()",
                    Prim::Int => "java.lang.Number.intValue()",
                    Prim::Long => "java.lang.Number.longValue()",
                    Prim::Float => "java.lang.Number.floatValue()",
                    Prim::Double => "java.lang.Number.doubleValue()",
                };
                return Err(npe(format!(
                    "Cannot invoke \"{m}\" because the return value of \"sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)\" is null"
                )));
            }
            // not a wrapper: `primitiveConversion` casts it to `Number` first
            return Err(cce(checkcast_message(&value_binary_name(&v), "java.lang.Number")));
        };
        if widens(from, to) {
            return Ok(convert(&v, to));
        }
        return Err(cce(format!("Cannot cast {} to {}", from.box_name(), to.box_name())).with_frames(&[CLASS_CAST_FRAME]));
    }
    if v.is_null() || t == Ty::DEF || t == Ty::OBJECT || vm.reg.instance_of(&v, t) {
        return Ok(v);
    }
    let to = if t.dims > 0 { crate::value::JArray::new(t.component(), 0).descriptor() } else { java_binary_name(t.class).to_string() };
    Err(cce(format!("Cannot cast {} to {}", value_binary_name(&v), to)).with_frames(&[CLASS_CAST_FRAME]))
}

/// The result of a `def` operator converted to the call site type.
pub fn cast_math_result(v: Value, to: Ty, explicit: bool) -> VR {
    if to == Ty::DEF || to == Ty::OBJECT || v.is_null() {
        return Ok(v);
    }
    let Some(tp) = Prim::of(to) else {
        return Ok(v);
    };
    let Some(from) = prim_of(&v) else {
        if to.is_primitive() {
            return Err(cce(format!("Cannot cast from: class {} to {}", java_binary_name(v.class()), tp.java_name())));
        }
        return Ok(v);
    };
    if !to.is_primitive() {
        // boxed target: the value must already be of that box
        if from == tp {
            return Ok(v);
        }
        return Err(cce(format!("Cannot cast from: {} to class {}", from.java_name(), tp.box_name())));
    }
    if from == tp {
        return Ok(v);
    }
    if explicit {
        if from == Prim::Bool || tp == Prim::Bool {
            return Err(cce(format!("Cannot cast {} to {}", from.java_name(), tp.java_name())));
        }
        return Ok(convert(&v, tp));
    }
    if widens(from, tp) {
        return Ok(convert(&v, tp));
    }
    Err(cce(format!("Cannot cast from: {} to {}", from.java_name(), tp.java_name())))
}

fn value_binary_name(v: &Value) -> String {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(a) => a.descriptor(),
            _ => java_binary_name(v.class()).to_string(),
        },
        _ => java_binary_name(v.class()).to_string(),
    }
}

/// The module part of a JVM `ClassCastException` message for a class.
fn module_of(name: &str) -> &'static str {
    if name.starts_with("java.") || name.starts_with('[') {
        "module java.base of loader 'bootstrap'"
    } else if name.starts_with("org.apache.lucene") {
        "module org.apache.lucene.core of loader 'app'"
    } else {
        "module org.elasticsearch.server of loader 'app'"
    }
}

/// The JVM's `checkcast` failure message.
pub fn checkcast_message(from: &str, to: &str) -> String {
    let (mf, mt) = (module_of(from), module_of(to));
    let from_k = if from.starts_with('[') { "" } else { "class " };
    let to_k = if to.starts_with('[') { "" } else { "class " };
    if mf == mt {
        format!("{from_k}{from} cannot be cast to {to_k}{to} ({from} and {to} are in {mf})")
    } else {
        format!("{from_k}{from} cannot be cast to {to_k}{to} ({from} is in {mf}; {to} is in {mt})")
    }
}

/// A reference downcast (`checkcast`).
pub fn check(vm: &mut Vm, v: Value, t: Ty) -> VR {
    if v.is_null() || vm.reg.instance_of(&v, t) {
        return Ok(v);
    }
    let to = if t.dims > 0 { crate::value::JArray::new(t.component(), 0).descriptor() } else { java_binary_name(t.boxed().class).to_string() };
    Err(cce(checkcast_message(&value_binary_name(&v), &to)))
}

/// Converts an argument passed from library code into a script function's parameter type
/// (the adaptation `LambdaMetafactory` performs).
pub fn to_param(vm: &mut Vm, v: Value, t: Ty) -> VR {
    if t == Ty::DEF || t == Ty::OBJECT {
        return Ok(v);
    }
    if let Some(p) = Prim::of(t)
        && t.is_primitive() {
            if v.is_null() {
                return Err(npe("Cannot unbox a null value"));
            }
            // the lambda class unboxes through `Number` for numeric parameters
            // (`((Number) o).intValue()`), `Boolean` and `Character` otherwise
            return match (prim_of(&v), p) {
                (Some(Prim::Bool), Prim::Bool) | (Some(Prim::Char), Prim::Char) => Ok(v),
                (Some(f), _) if f != Prim::Bool && f != Prim::Char && p != Prim::Bool && p != Prim::Char => Ok(convert(&v, p)),
                (None, _) if p != Prim::Bool && p != Prim::Char && v.other_number().is_some() => {
                    let (d, l) = v.other_number().unwrap();
                    Ok(match p {
                        Prim::Double => Value::Double(d),
                        Prim::Float => Value::Float(d as f32),
                        _ => convert(&Value::Long(l), p),
                    })
                }
                _ => {
                    let to = match p {
                        Prim::Bool => "java.lang.Boolean",
                        Prim::Char => "java.lang.Character",
                        _ => "java.lang.Number",
                    };
                    Err(cce(checkcast_message(&value_binary_name(&v), to)))
                }
            };
        }
    if v.is_null() || vm.reg.instance_of(&v, t) {
        return Ok(v);
    }
    // reference parameters adapt through `Class.cast`
    if vm.reg.functional(t).is_some()
        && let Value::Obj(o) = &v
        && matches!(&**o, Obj::Function(_))
    {
        return Ok(v);
    }
    let to = if t.dims > 0 { crate::value::JArray::new(t.component(), 0).descriptor() } else { java_binary_name(t.boxed().class).to_string() };
    Err(cce(format!("Cannot cast {} to {}", value_binary_name(&v), to)).with_frames(&["java.base/java.lang.Class.cast(Class.java:3495)"]))
}

/// Converts a value returned by a function value to what a library method expects from an
/// interface method (`Predicate.test` → boolean, …).
pub fn from_return(v: Value, t: Ty) -> VR {
    if t == Ty::DEF || t == Ty::OBJECT || t == Ty::VOID {
        return Ok(v);
    }
    if let Some(p) = Prim::of(t)
        && t.is_primitive() {
            if v.is_null() {
                return Err(npe("Cannot unbox a null value"));
            }
            return match prim_of(&v) {
                Some(f) if widens(f, p) => Ok(convert(&v, p)),
                Some(f) if f != Prim::Bool && p != Prim::Bool => Ok(convert(&v, p)),
                _ => Err(cce(checkcast_message(&value_binary_name(&v), p.box_name()))),
            };
        }
    Ok(v)
}

/// Truthiness for library methods consuming a `boolean`-returning function.
pub fn as_bool(v: &Value) -> Result<bool, Thrown> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Null => Err(npe("Cannot invoke \"java.lang.Boolean.booleanValue()\" because the return value is null")),
        other => Err(cce(checkcast_message(&value_binary_name(other), "java.lang.Boolean"))),
    }
}

pub fn as_int(v: &Value) -> Result<i32, Thrown> {
    match v {
        Value::Int(i) => Ok(*i),
        Value::Byte(_) | Value::Short(_) | Value::Char(_) => Ok(v.as_i64().unwrap() as i32),
        Value::Null => Err(npe("Cannot unbox a null value")),
        other => Err(cce(checkcast_message(&value_binary_name(other), "java.lang.Integer"))),
    }
}

pub fn as_long(v: &Value) -> Result<i64, Thrown> {
    match v {
        Value::Long(i) => Ok(*i),
        Value::Int(_) | Value::Byte(_) | Value::Short(_) | Value::Char(_) => Ok(v.as_i64().unwrap()),
        Value::Null => Err(npe("Cannot unbox a null value")),
        other => Err(cce(checkcast_message(&value_binary_name(other), "java.lang.Long"))),
    }
}

pub fn as_double(v: &Value) -> Result<f64, Thrown> {
    match v {
        Value::Null => Err(npe("Cannot unbox a null value")),
        v if v.as_f64().is_some() && !matches!(v, Value::Bool(_)) => Ok(v.as_f64().unwrap()),
        other => Err(cce(checkcast_message(&value_binary_name(other), "java.lang.Double"))),
    }
}
