//! Function values: lambdas, method references and the `java.util.function` interfaces.
//!
//! A function value records what to call and, when it was created against a known
//! functional interface, which one (so `instanceof` and `Debug.explain` see it). Calling one
//! from library code converts arguments to the target's parameter types and returns its result
//! for the caller to convert to the interface method's return type. Combinators (`andThen`,
//! `negate`, `Comparator.comparing`, …) are native closures over other function values.

use std::rc::Rc;

use crate::cast;
use crate::ir::FuncRefSpec;
use crate::lookup::{Builder, MethodId, MethodKind, Registry};
use crate::types::{cls, ClassId, Ty};
use crate::value::{JArray, Obj, Value};
use crate::vm::{iae, npe, Thrown, Vm, VR};

type NativeFn = dyn for<'a, 'b> Fn(&'b mut Vm<'a>, &[Value]) -> VR;

pub enum Target {
    /// A script function (lambda body or `this::f`) with captured values.
    Script { f: u32, captures: Vec<Value> },
    /// A library method; instance methods take the receiver as the first argument unless bound.
    Method { m: MethodId, bound: Option<Value> },
    /// `Class::name` whose arity decides between a static and an instance method.
    Unresolved { class: ClassId, name: Rc<str>, sym: Rc<str> },
    /// `x::name` on a `def` variable.
    Capture { recv: Value, name: Rc<str> },
    /// `this::name` resolved by arity when called.
    Local { name: Rc<str> },
    Ctor { class: ClassId },
    NewArray { ty: Ty },
    Native(Rc<NativeFn>),
}

pub struct FnRef {
    pub iface: Option<ClassId>,
    pub target: Target,
}

impl FnRef {
    pub fn class(&self) -> ClassId {
        self.iface.unwrap_or(cls::LAMBDA)
    }
}

/// Compile-time description of a method reference.
#[derive(Debug, Clone)]
pub enum RefSpec {
    Class { class: ClassId, name: Box<str>, iface: Option<ClassId>, sym: Box<str> },
    Ctor { class: ClassId, iface: Option<ClassId> },
    NewArray { ty: Ty, iface: Option<ClassId> },
    Local { name: Box<str>, iface: Option<ClassId> },
    Capture { name: Box<str>, iface: Option<ClassId>, typed: Option<Ty> },
}

/// Lowers an analyzed reference; returns the spec and the captured slot, if any.
pub fn ref_spec(reg: &Registry, s: &FuncRefSpec) -> (RefSpec, Option<u32>) {
    let _ = reg;
    match s {
        FuncRefSpec::Class { class, name, iface, sym } => (RefSpec::Class { class: *class, name: name.clone(), iface: *iface, sym: sym.clone() }, None),
        FuncRefSpec::Ctor { class, iface } => (RefSpec::Ctor { class: *class, iface: *iface }, None),
        FuncRefSpec::NewArray { ty, iface } => (RefSpec::NewArray { ty: *ty, iface: *iface }, None),
        FuncRefSpec::Local { name, iface } => (RefSpec::Local { name: name.clone(), iface: *iface }, None),
        FuncRefSpec::Capture { slot, name, iface } => (RefSpec::Capture { name: name.clone(), iface: *iface, typed: None }, Some(*slot)),
    }
}

pub fn lambda(f: u32, captures: Vec<Value>, iface: Option<ClassId>) -> Value {
    Value::obj(Obj::Function(FnRef { iface, target: Target::Script { f, captures } }))
}

pub fn native(iface: Option<ClassId>, f: impl for<'a, 'b> Fn(&'b mut Vm<'a>, &[Value]) -> VR + 'static) -> Value {
    Value::obj(Obj::Function(FnRef { iface, target: Target::Native(Rc::new(f)) }))
}

pub fn make_ref(vm: &mut Vm, spec: u32, cap: Value) -> VR {
    let prog = vm.prog;
    let s = &prog.funcrefs[spec as usize];
    let (iface, target) = match s {
        RefSpec::Class { class, name, iface, sym } => (*iface, Target::Unresolved { class: *class, name: Rc::from(&**name), sym: Rc::from(&**sym) }),
        RefSpec::Ctor { class, iface } => (*iface, Target::Ctor { class: *class }),
        RefSpec::NewArray { ty, iface } => (*iface, Target::NewArray { ty: *ty }),
        RefSpec::Local { name, iface } => (*iface, Target::Local { name: Rc::from(&**name) }),
        RefSpec::Capture { name, iface, .. } => {
            if cap.is_null() {
                return Err(npe("Cannot invoke \"Object.getClass()\" because \"receiver\" is null"));
            }
            let target = Target::Capture { recv: cap, name: Rc::from(&**name) };
            // a `def` variable's reference resolves when it is created
            if let Some(i) = iface {
                check_target(vm, &target, Ty::of(*i))?;
            }
            (*iface, target)
        }
    };
    Ok(Value::obj(Obj::Function(FnRef { iface, target })))
}

/// Linking a lambda or method reference passed to a `def` call (`Def.lookupMethod` with a
/// recipe): once the method is known, so is the functional interface of the parameter, and
/// `FunctionRef.create` resolves the reference against it (without the compile-time casts),
/// failing the call before the method runs.
pub fn link_ref(vm: &mut Vm, arg: &Value, param: Ty) -> Result<(), Thrown> {
    let Value::Obj(o) = arg else { return Ok(()) };
    let Obj::Function(fr) = &**o else { return Ok(()) };
    if fr.iface.is_some() {
        return Ok(());
    }
    check_target(vm, &fr.target, param)
}

/// `FunctionRef.create` at run time for `target` against the functional interface `param`.
fn check_target(vm: &mut Vm, target: &Target, param: Ty) -> Result<(), Thrown> {
    let reg = vm.reg;
    let Some(im) = reg.functional(param) else { return Ok(()) };
    let im = reg.method(im);
    let n = im.params.len();
    let iface = crate::lookup::type_name(param);
    let not_found = |what: String, suffix: &str| iae(format!("function reference [{what}/{n}] matching [{iface}, {}/{n}] not found{suffix}", im.name));
    let lambda_ret = |ret: Ty| -> Result<(), Thrown> {
        if im.ret != Ty::VOID && ret == Ty::VOID {
            return Err(crate::vm::Throw::new(
                cls::LAMBDA_CONVERSION_EXCEPTION,
                Some(format!("lambda expects return type [{}], but found return type [void]", crate::lookup::type_name(im.ret))),
            ));
        }
        Ok(())
    };
    match target {
        Target::Script { f, captures } => {
            let func = &vm.prog.functions[*f as usize];
            if func.params.len() != n {
                let idx = func.name.strip_prefix("lambda$").unwrap_or(&func.name);
                // the local function key counts the captures too
                return Err(iae(format!(
                    "function reference [this::lambda$synthetic${idx}/{}] matching [{iface}, {}/{n}] not found due to an incorrect number of arguments",
                    captures.len() + n,
                    im.name
                )));
            }
        }
        Target::Unresolved { class, name, sym } => {
            let ret = if let Some(m) = reg.lookup_method(*class, true, name, n) {
                reg.method(m).ret
            } else if let Some(m) = n.checked_sub(1).and_then(|k| reg.lookup_method(*class, false, name, k)) {
                reg.method(m).ret
            } else {
                return Err(not_found(format!("{sym}::{name}"), ""));
            };
            lambda_ret(ret)?;
        }
        Target::Capture { recv, name } => {
            let class = recv.class();
            match reg.lookup_method(class, false, name, n) {
                Some(m) => lambda_ret(reg.method(m).ret)?,
                None => return Err(not_found(format!("{}::{name}", crate::lookup::type_name(recv.runtime_ty())), "")),
            }
        }
        Target::Local { name } => {
            if !vm.prog.functions.iter().any(|f| *f.name == **name && f.params.len() == n) {
                return Err(not_found(format!("this::{name}"), ""));
            }
        }
        Target::Ctor { class } => {
            if reg.lookup_ctor(*class, n).is_none() {
                return Err(not_found(format!("{}::new", crate::lookup::class_name(*class)), ""));
            }
        }
        Target::Method { .. } | Target::NewArray { .. } | Target::Native(_) => {}
    }
    Ok(())
}

/// Calls a function value with arguments.
pub fn call(vm: &mut Vm, f: &Value, args: &[Value]) -> VR {
    let fr = match f {
        Value::Obj(o) => match &**o {
            Obj::Function(fr) => fr,
            _ => return Err(crate::vm::cce(format!("{} is not a function", crate::lookup::type_name(f.runtime_ty())))),
        },
        Value::Null => return Err(npe("Cannot invoke a function because the function value is null")),
        _ => return Err(crate::vm::cce(format!("{} is not a function", crate::lookup::type_name(f.runtime_ty())))),
    };
    match &fr.target {
        Target::Script { f: fi, captures } => call_script(vm, *fi, captures, args),
        Target::Native(n) => {
            let n = n.clone();
            n(vm, args)
        }
        Target::Method { m, bound } => {
            let m = *m;
            let mut full: Vec<Value> = Vec::with_capacity(args.len() + 1);
            if let Some(b) = bound {
                full.push(b.clone());
            }
            full.extend_from_slice(args);
            invoke_method(vm, m, full)
        }
        Target::Unresolved { class, name, .. } => {
            let reg = vm.reg;
            let n = args.len();
            if let Some(m) = reg.lookup_method(*class, true, name, n) {
                return invoke_method(vm, m, args.to_vec());
            }
            if n > 0
                && let Some(m) = reg.lookup_method(*class, false, name, n - 1) {
                    return invoke_method(vm, m, args.to_vec());
                }
            Err(iae(format!("function reference [{}::{name}/{n}] not found", crate::lookup::class_name(*class))))
        }
        Target::Capture { recv, name } => {
            let reg = vm.reg;
            let class = recv.class();
            let n = args.len();
            match reg.lookup_method(class, false, name, n) {
                Some(m) => {
                    let mut full = vec![recv.clone()];
                    full.extend_from_slice(args);
                    invoke_method(vm, m, full)
                }
                None => Err(iae(format!("dynamic method [{}, {name}/{n}] not found", crate::lookup::type_name(recv.runtime_ty())))),
            }
        }
        Target::Local { name } => {
            let prog = vm.prog;
            let fi = prog.functions.iter().position(|f| *f.name == **name && f.params.len() == args.len());
            match fi {
                Some(fi) => call_script(vm, fi as u32, &[], args),
                None => Err(iae(format!("function reference [this::{name}/{}] not found", args.len()))),
            }
        }
        Target::Ctor { class } => {
            let reg = vm.reg;
            match reg.lookup_ctor(*class, args.len()) {
                Some(m) => invoke_method(vm, m, args.to_vec()),
                None => Err(iae(format!("constructor [{}, <init>/{}] not found", crate::lookup::class_name(*class), args.len()))),
            }
        }
        Target::NewArray { ty } => {
            let n = args.first().and_then(|v| v.as_i64()).unwrap_or(0);
            crate::dynamic::new_array(*ty, &[n])
        }
    }
}

fn call_script(vm: &mut Vm, fi: u32, captures: &[Value], args: &[Value]) -> VR {
    let prog = vm.prog;
    let func = &prog.functions[fi as usize];
    if func.params.len() != args.len() {
        return Err(iae(format!(
            "Incorrect number of parameters for [{}]: expected [{}] but found [{}]",
            func.name,
            func.params.len(),
            args.len()
        )));
    }
    let mut conv: Vec<Value> = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        conv.push(cast::to_param(vm, a.clone(), func.params[i])?);
    }
    vm.call_function(fi, &conv, captures)
}

fn invoke_method(vm: &mut Vm, m: MethodId, mut args: Vec<Value>) -> VR {
    let reg = vm.reg;
    let mi = reg.method(m);
    let offset = if mi.kind == MethodKind::Instance { 1 } else { 0 };
    if offset == 1
        && (args.is_empty() || args[0].is_null()) {
            return Err(crate::dynamic::null_receiver(reg, m, None));
        }
    for (i, pt) in mi.params.iter().enumerate() {
        if let Some(a) = args.get_mut(i + offset) {
            let v = std::mem::take(a);
            *a = crate::dynamic::arg_to(vm, v, *pt)?;
        }
    }
    vm.call_native(mi.imp, m, &mut args)
}

/// Calls a function and converts its result to `boolean`.
pub fn call_bool(vm: &mut Vm, f: &Value, args: &[Value]) -> Result<bool, Thrown> {
    let r = call(vm, f, args)?;
    cast::as_bool(&r)
}

pub fn call_int(vm: &mut Vm, f: &Value, args: &[Value]) -> Result<i32, Thrown> {
    let r = call(vm, f, args)?;
    match cast::from_return(r, Ty::INT)? {
        Value::Int(i) => Ok(i),
        _ => Ok(0),
    }
}

pub fn call_long(vm: &mut Vm, f: &Value, args: &[Value]) -> Result<i64, Thrown> {
    let r = call(vm, f, args)?;
    match cast::from_return(r, Ty::LONG)? {
        Value::Long(i) => Ok(i),
        _ => Ok(0),
    }
}

pub fn call_double(vm: &mut Vm, f: &Value, args: &[Value]) -> Result<f64, Thrown> {
    let r = call(vm, f, args)?;
    match cast::from_return(r, Ty::DOUBLE)? {
        Value::Double(d) => Ok(d),
        _ => Ok(0.0),
    }
}

pub fn is_function(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(&**o, Obj::Function(_)))
}

// ------------------------------------------------------------------------------ comparators

/// Compares with an optional comparator (`null` means natural ordering).
pub fn compare_with(vm: &mut Vm, cmp: &Value, a: &Value, b: &Value) -> Result<std::cmp::Ordering, Thrown> {
    if cmp.is_null() {
        return natural_compare(a, b);
    }
    let r = call_int(vm, cmp, &[a.clone(), b.clone()])?;
    Ok(r.cmp(&0))
}

/// `Comparable.compareTo` for values that implement it.
pub fn natural_compare(a: &Value, b: &Value) -> Result<std::cmp::Ordering, Thrown> {
    use std::cmp::Ordering;
    use Value::*;
    // `a.compareTo(b)` enters the compiler's bridge `compareTo(Object)`, whose checkcast fails;
    // javac gives the bridge the line where the class declaration (its annotations) starts
    let cce_pair = |a: &Value, b: &Value| {
        let t = crate::vm::cce(crate::cast::checkcast_message(
            crate::lookup::java_binary_name(b.class()),
            crate::lookup::java_binary_name(a.class()),
        ));
        let bridge = match a {
            Int(_) => Some("java.base/java.lang.Integer.compareTo(Integer.java:73)"),
            Long(_) => Some("java.base/java.lang.Long.compareTo(Long.java:72)"),
            Double(_) => Some("java.base/java.lang.Double.compareTo(Double.java:359)"),
            Float(_) => Some("java.base/java.lang.Float.compareTo(Float.java:75)"),
            Short(_) => Some("java.base/java.lang.Short.compareTo(Short.java:61)"),
            Byte(_) => Some("java.base/java.lang.Byte.compareTo(Byte.java:61)"),
            Char(_) => Some("java.base/java.lang.Character.compareTo(Character.java:221)"),
            Bool(_) => Some("java.base/java.lang.Boolean.compareTo(Boolean.java:56)"),
            Str(_) => Some("java.base/java.lang.String.compareTo(String.java:173)"),
            _ => None,
        };
        match bridge {
            Some(f) => t.with_frames(&[f]),
            None => t,
        }
    };
    if a.is_null() {
        return Err(npe("Cannot invoke \"java.lang.Comparable.compareTo(Object)\" because \"k1\" is null"));
    }
    if b.is_null() {
        // `a.compareTo(null)` through the bridge method: the receiver's own NPE
        let (class, field, line, bridge) = match a {
            Int(_) => ("Integer", "anotherInteger", 1377, 73),
            Long(_) => ("Long", "anotherLong", 1360, 72),
            Double(_) => ("Double", "anotherDouble", 1487, 359),
            Float(_) => ("Float", "anotherFloat", 1273, 75),
            Short(_) => ("Short", "anotherShort", 505, 61),
            Byte(_) => ("Byte", "anotherByte", 499, 61),
            Char(_) => ("Character", "anotherCharacter", 12209, 221),
            Bool(_) => ("Boolean", "b", 290, 56),
            Str(_) => ("String", "anotherString", 2406, 173),
            _ => return Err(npe("Cannot invoke \"java.lang.Comparable.compareTo(Object)\" because \"k2\" is null")),
        };
        return Err(npe(format!("Cannot read field \"value\" because \"{field}\" is null"))
            .with_frames(&[&format!("java.base/java.lang.{class}.compareTo({class}.java:{line})"), &format!("java.base/java.lang.{class}.compareTo({class}.java:{bridge})")]));
    }
    Ok(match (a, b) {
        (Int(x), Int(y)) => x.cmp(y),
        (Long(x), Long(y)) => x.cmp(y),
        (Short(x), Short(y)) => x.cmp(y),
        (Byte(x), Byte(y)) => x.cmp(y),
        (Char(x), Char(y)) => x.cmp(y),
        (Bool(x), Bool(y)) => x.cmp(y),
        (Double(x), Double(y)) => java_double_cmp(*x, *y),
        (Float(x), Float(y)) => java_double_cmp(*x as f64, *y as f64),
        (Str(x), Str(y)) => crate::java::compare_strings(x.as_str(), y.as_str()).cmp(&0),
        (Obj(x), Obj(y)) => match (&**x, &**y) {
            (crate::value::Obj::Time(p), crate::value::Obj::Time(q)) => match p.compare(q) {
                Some(o) => o,
                None => return Err(cce_pair(a, b)),
            },
            (crate::value::Obj::BigInt(p), crate::value::Obj::BigInt(q)) => p.cmp(q),
            (crate::value::Obj::BigDec(p), crate::value::Obj::BigDec(q)) => p.compare(q),
            (crate::value::Obj::Uuid(m1, l1), crate::value::Obj::Uuid(m2, l2)) => ((*m1 as i64), (*l1 as i64)).cmp(&((*m2 as i64), (*l2 as i64))),
            (crate::value::Obj::Legacy(p), crate::value::Obj::Legacy(q)) => match p.compare(q) {
                Some(o) => o,
                None => return Err(cce_pair(a, b)),
            },
            (crate::value::Obj::Es(p), _) => match p.compare(b) {
                Some(o) => o,
                None => return Err(cce_pair(a, b)),
            },
            _ => {
                return Err(crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(a.class()), "java.lang.Comparable")))
            }
        },
        (List(_) | Map(_) | Set(_), _) => {
            return Err(crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(a.class()), "java.lang.Comparable")))
        }
        _ => {
            if std::mem::discriminant(a) == std::mem::discriminant(b) {
                Ordering::Equal
            } else {
                return Err(cce_pair(a, b));
            }
        }
    })
}

/// `Double.compare`: -0.0 < 0.0 and NaN is greater than everything.
pub fn java_double_cmp(x: f64, y: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    if x < y {
        return Less;
    }
    if x > y {
        return Greater;
    }
    let (a, b) = (if x.is_nan() { 0x7ff8000000000000i64 } else { x.to_bits() as i64 }, if y.is_nan() { 0x7ff8000000000000i64 } else { y.to_bits() as i64 });
    a.cmp(&b)
}

/// Sorts with a comparator (stable, like `List.sort`); errors from the comparator propagate.
/// Sorts with a comparator (`null`: natural ordering) the way `Arrays.sort` does (TimSort, see
/// [`crate::sort`]); errors from the comparator propagate, leaving `items` partially sorted.
pub fn sort_values(vm: &mut Vm, items: &mut Vec<Value>, cmp: &Value) -> Result<(), Thrown> {
    if cmp.is_null() {
        crate::sort::timsort(items, crate::sort::Flavor::Natural, &mut |a, b| natural_compare(a, b))
    } else {
        crate::sort::timsort(items, crate::sort::Flavor::Comparator, &mut |a, b| compare_with(vm, cmp, a, b))
    }
}

// ------------------------------------------------------------------------------ registration

fn arg_fn(a: &[Value], i: usize) -> Value {
    a.get(i).cloned().unwrap_or(Value::Null)
}

/// Invokes the receiver function with the remaining arguments (functional interface method).
fn invoke_def(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[0].clone();
    call(vm, &f, &a[1..])
}
fn invoke_bool(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[0].clone();
    Ok(Value::Bool(call_bool(vm, &f, &a[1..])?))
}
fn invoke_int(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[0].clone();
    Ok(Value::Int(call_int(vm, &f, &a[1..])?))
}
fn invoke_long(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[0].clone();
    Ok(Value::Long(call_long(vm, &f, &a[1..])?))
}
fn invoke_double(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[0].clone();
    Ok(Value::Double(call_double(vm, &f, &a[1..])?))
}
fn invoke_void(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[0].clone();
    call(vm, &f, &a[1..])?;
    Ok(Value::Null)
}

fn and_then(vm: &mut Vm, a: &mut [Value]) -> VR {
    let _ = vm;
    let (f, g) = (arg_fn(a, 0), arg_fn(a, 1));
    let iface = fn_iface(&f);
    Ok(native(iface, move |vm, args| {
        let r = call(vm, &f, args)?;
        call(vm, &g, &[r])
    }))
}
fn consumer_and_then(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, g) = (arg_fn(a, 0), arg_fn(a, 1));
    let iface = fn_iface(&f);
    Ok(native(iface, move |vm, args| {
        call(vm, &f, args)?;
        call(vm, &g, args)?;
        Ok(Value::Null)
    }))
}
fn compose(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, g) = (arg_fn(a, 0), arg_fn(a, 1));
    let iface = fn_iface(&f);
    Ok(native(iface, move |vm, args| {
        let r = call(vm, &g, args)?;
        call(vm, &f, &[r])
    }))
}
fn identity(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(native(Some(cls::FUNCTION), |_, args| Ok(arg_fn(args, 0))))
}
fn negate(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = arg_fn(a, 0);
    let iface = fn_iface(&f);
    Ok(native(iface, move |vm, args| Ok(Value::Bool(!call_bool(vm, &f, args)?))))
}
fn pred_and(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, g) = (arg_fn(a, 0), arg_fn(a, 1));
    let iface = fn_iface(&f);
    Ok(native(iface, move |vm, args| Ok(Value::Bool(call_bool(vm, &f, args)? && call_bool(vm, &g, args)?))))
}
fn pred_or(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, g) = (arg_fn(a, 0), arg_fn(a, 1));
    let iface = fn_iface(&f);
    Ok(native(iface, move |vm, args| Ok(Value::Bool(call_bool(vm, &f, args)? || call_bool(vm, &g, args)?))))
}
fn is_equal(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = arg_fn(a, 0);
    Ok(native(Some(cls::PREDICATE), move |_, args| Ok(Value::Bool(t.java_equals(&arg_fn(args, 0))))))
}
fn min_by(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = arg_fn(a, 0);
    Ok(native(Some(cls::BINARY_OPERATOR), move |vm, args| {
        let (x, y) = (arg_fn(args, 0), arg_fn(args, 1));
        Ok(if compare_with(vm, &c, &x, &y)? != std::cmp::Ordering::Greater { x } else { y })
    }))
}
fn max_by(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = arg_fn(a, 0);
    Ok(native(Some(cls::BINARY_OPERATOR), move |vm, args| {
        let (x, y) = (arg_fn(args, 0), arg_fn(args, 1));
        Ok(if compare_with(vm, &c, &x, &y)? != std::cmp::Ordering::Less { x } else { y })
    }))
}

fn fn_iface(v: &Value) -> Option<ClassId> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Function(f) => f.iface,
            _ => None,
        },
        _ => None,
    }
}

fn cmp_value(o: std::cmp::Ordering) -> Value {
    Value::Int(match o {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

fn comparator(f: impl for<'a, 'b> Fn(&'b mut Vm<'a>, &Value, &Value) -> Result<std::cmp::Ordering, Thrown> + 'static) -> Value {
    native(Some(cls::COMPARATOR), move |vm, args| Ok(cmp_value(f(vm, &arg_fn(args, 0), &arg_fn(args, 1))?)))
}

fn natural_order(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(comparator(|_, a, b| natural_compare(a, b)))
}
fn reverse_order(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(comparator(|_, a, b| natural_compare(b, a)))
}
fn reversed(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = arg_fn(a, 0);
    Ok(comparator(move |vm, x, y| compare_with(vm, &c, y, x)))
}
fn comparing(_: &mut Vm, a: &mut [Value]) -> VR {
    let k = arg_fn(a, 0);
    Ok(comparator(move |vm, x, y| {
        let kx = call(vm, &k, std::slice::from_ref(x))?;
        let ky = call(vm, &k, std::slice::from_ref(y))?;
        natural_compare(&kx, &ky)
    }))
}
fn comparing2(_: &mut Vm, a: &mut [Value]) -> VR {
    let (k, c) = (arg_fn(a, 0), arg_fn(a, 1));
    Ok(comparator(move |vm, x, y| {
        let kx = call(vm, &k, std::slice::from_ref(x))?;
        let ky = call(vm, &k, std::slice::from_ref(y))?;
        compare_with(vm, &c, &kx, &ky)
    }))
}
fn comparing_double(_: &mut Vm, a: &mut [Value]) -> VR {
    let k = arg_fn(a, 0);
    Ok(comparator(move |vm, x, y| Ok(java_double_cmp(call_double(vm, &k, std::slice::from_ref(x))?, call_double(vm, &k, std::slice::from_ref(y))?))))
}
fn comparing_int(_: &mut Vm, a: &mut [Value]) -> VR {
    let k = arg_fn(a, 0);
    Ok(comparator(move |vm, x, y| Ok(call_int(vm, &k, std::slice::from_ref(x))?.cmp(&call_int(vm, &k, std::slice::from_ref(y))?))))
}
fn comparing_long(_: &mut Vm, a: &mut [Value]) -> VR {
    let k = arg_fn(a, 0);
    Ok(comparator(move |vm, x, y| Ok(call_long(vm, &k, std::slice::from_ref(x))?.cmp(&call_long(vm, &k, std::slice::from_ref(y))?))))
}
fn then_comparing(_: &mut Vm, a: &mut [Value]) -> VR {
    let (c1, c2) = (arg_fn(a, 0), arg_fn(a, 1));
    Ok(comparator(move |vm, x, y| {
        let o = compare_with(vm, &c1, x, y)?;
        if o != std::cmp::Ordering::Equal {
            return Ok(o);
        }
        compare_with(vm, &c2, x, y)
    }))
}
fn then_comparing2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let c1 = arg_fn(a, 0);
    let mut rest = [arg_fn(a, 1), arg_fn(a, 2)];
    let c2 = comparing2(vm, &mut rest)?;
    let mut both = [c1, c2];
    then_comparing(vm, &mut both)
}
fn then_comparing_double(vm: &mut Vm, a: &mut [Value]) -> VR {
    let c1 = arg_fn(a, 0);
    let mut rest = [arg_fn(a, 1)];
    let c2 = comparing_double(vm, &mut rest)?;
    then_comparing(vm, &mut [c1, c2])
}
fn then_comparing_int(vm: &mut Vm, a: &mut [Value]) -> VR {
    let c1 = arg_fn(a, 0);
    let mut rest = [arg_fn(a, 1)];
    let c2 = comparing_int(vm, &mut rest)?;
    then_comparing(vm, &mut [c1, c2])
}
fn then_comparing_long(vm: &mut Vm, a: &mut [Value]) -> VR {
    let c1 = arg_fn(a, 0);
    let mut rest = [arg_fn(a, 1)];
    let c2 = comparing_long(vm, &mut rest)?;
    then_comparing(vm, &mut [c1, c2])
}
fn nulls_first(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = arg_fn(a, 0);
    Ok(comparator(move |vm, x, y| {
        use std::cmp::Ordering::*;
        match (x.is_null(), y.is_null()) {
            (true, true) => Ok(Equal),
            (true, false) => Ok(Less),
            (false, true) => Ok(Greater),
            _ => {
                if c.is_null() {
                    Ok(Equal)
                } else {
                    compare_with(vm, &c, x, y)
                }
            }
        }
    }))
}
fn nulls_last(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = arg_fn(a, 0);
    Ok(comparator(move |vm, x, y| {
        use std::cmp::Ordering::*;
        match (x.is_null(), y.is_null()) {
            (true, true) => Ok(Equal),
            (true, false) => Ok(Greater),
            (false, true) => Ok(Less),
            _ => {
                if c.is_null() {
                    Ok(Equal)
                } else {
                    compare_with(vm, &c, x, y)
                }
            }
        }
    }))
}
fn fn_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(a[0].java_equals(&a[1])))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    let fi = |b: &mut Builder, c, sig: &str, imp| {
        b.interface(c, &[]);
        b.functional(c, sig, imp);
    };
    fi(b, BI_CONSUMER, "void accept(def,def)", invoke_void);
    b.m(BI_CONSUMER, "BiConsumer andThen(BiConsumer)", consumer_and_then);
    fi(b, BI_FUNCTION, "def apply(def,def)", invoke_def);
    b.m(BI_FUNCTION, "BiFunction andThen(Function)", and_then);
    b.interface(BINARY_OPERATOR, &[BI_FUNCTION]);
    b.s(BINARY_OPERATOR, "BinaryOperator maxBy(Comparator)", max_by);
    b.s(BINARY_OPERATOR, "BinaryOperator minBy(Comparator)", min_by);
    fi(b, BI_PREDICATE, "boolean test(def,def)", invoke_bool);
    b.m(BI_PREDICATE, "BiPredicate and(BiPredicate)", pred_and);
    b.m(BI_PREDICATE, "BiPredicate negate()", negate);
    b.m(BI_PREDICATE, "BiPredicate or(BiPredicate)", pred_or);
    fi(b, BOOLEAN_SUPPLIER, "boolean getAsBoolean()", invoke_bool);
    fi(b, CONSUMER, "void accept(def)", invoke_void);
    b.m(CONSUMER, "Consumer andThen(Consumer)", consumer_and_then);
    fi(b, DOUBLE_BINARY_OPERATOR, "double applyAsDouble(double,double)", invoke_double);
    fi(b, DOUBLE_CONSUMER, "void accept(double)", invoke_void);
    b.m(DOUBLE_CONSUMER, "DoubleConsumer andThen(DoubleConsumer)", consumer_and_then);
    fi(b, DOUBLE_FUNCTION, "def apply(double)", invoke_def);
    fi(b, DOUBLE_PREDICATE, "boolean test(double)", invoke_bool);
    b.m(DOUBLE_PREDICATE, "DoublePredicate and(DoublePredicate)", pred_and);
    b.m(DOUBLE_PREDICATE, "DoublePredicate negate()", negate);
    b.m(DOUBLE_PREDICATE, "DoublePredicate or(DoublePredicate)", pred_or);
    fi(b, DOUBLE_SUPPLIER, "double getAsDouble()", invoke_double);
    fi(b, DOUBLE_TO_INT_FUNCTION, "int applyAsInt(double)", invoke_int);
    fi(b, DOUBLE_TO_LONG_FUNCTION, "long applyAsLong(double)", invoke_long);
    fi(b, DOUBLE_UNARY_OPERATOR, "double applyAsDouble(double)", invoke_double);
    b.m(DOUBLE_UNARY_OPERATOR, "DoubleUnaryOperator andThen(DoubleUnaryOperator)", and_then);
    b.m(DOUBLE_UNARY_OPERATOR, "DoubleUnaryOperator compose(DoubleUnaryOperator)", compose);
    b.s(DOUBLE_UNARY_OPERATOR, "DoubleUnaryOperator identity()", identity);
    fi(b, FUNCTION, "def apply(def)", invoke_def);
    b.m(FUNCTION, "Function andThen(Function)", and_then);
    b.m(FUNCTION, "Function compose(Function)", compose);
    b.s(FUNCTION, "Function identity()", identity);
    fi(b, INT_BINARY_OPERATOR, "int applyAsInt(int,int)", invoke_int);
    fi(b, INT_CONSUMER, "void accept(int)", invoke_void);
    b.m(INT_CONSUMER, "IntConsumer andThen(IntConsumer)", consumer_and_then);
    fi(b, INT_FUNCTION, "def apply(int)", invoke_def);
    fi(b, INT_PREDICATE, "boolean test(int)", invoke_bool);
    b.m(INT_PREDICATE, "IntPredicate and(IntPredicate)", pred_and);
    b.m(INT_PREDICATE, "IntPredicate negate()", negate);
    b.m(INT_PREDICATE, "IntPredicate or(IntPredicate)", pred_or);
    fi(b, INT_SUPPLIER, "int getAsInt()", invoke_int);
    fi(b, INT_TO_DOUBLE_FUNCTION, "double applyAsDouble(int)", invoke_double);
    fi(b, INT_TO_LONG_FUNCTION, "long applyAsLong(int)", invoke_long);
    fi(b, INT_UNARY_OPERATOR, "int applyAsInt(int)", invoke_int);
    b.m(INT_UNARY_OPERATOR, "IntUnaryOperator andThen(IntUnaryOperator)", and_then);
    b.m(INT_UNARY_OPERATOR, "IntUnaryOperator compose(IntUnaryOperator)", compose);
    b.s(INT_UNARY_OPERATOR, "IntUnaryOperator identity()", identity);
    fi(b, LONG_BINARY_OPERATOR, "long applyAsLong(long,long)", invoke_long);
    fi(b, LONG_CONSUMER, "void accept(long)", invoke_void);
    b.m(LONG_CONSUMER, "LongConsumer andThen(LongConsumer)", consumer_and_then);
    fi(b, LONG_FUNCTION, "def apply(long)", invoke_def);
    fi(b, LONG_PREDICATE, "boolean test(long)", invoke_bool);
    b.m(LONG_PREDICATE, "LongPredicate and(LongPredicate)", pred_and);
    b.m(LONG_PREDICATE, "LongPredicate negate()", negate);
    b.m(LONG_PREDICATE, "LongPredicate or(LongPredicate)", pred_or);
    fi(b, LONG_SUPPLIER, "long getAsLong()", invoke_long);
    fi(b, LONG_TO_DOUBLE_FUNCTION, "double applyAsDouble(long)", invoke_double);
    fi(b, LONG_TO_INT_FUNCTION, "int applyAsInt(long)", invoke_int);
    fi(b, LONG_UNARY_OPERATOR, "long applyAsLong(long)", invoke_long);
    b.m(LONG_UNARY_OPERATOR, "LongUnaryOperator andThen(LongUnaryOperator)", and_then);
    b.m(LONG_UNARY_OPERATOR, "LongUnaryOperator compose(LongUnaryOperator)", compose);
    b.s(LONG_UNARY_OPERATOR, "LongUnaryOperator identity()", identity);
    fi(b, OBJ_DOUBLE_CONSUMER, "void accept(def,double)", invoke_void);
    fi(b, OBJ_INT_CONSUMER, "void accept(def,int)", invoke_void);
    fi(b, OBJ_LONG_CONSUMER, "void accept(def,long)", invoke_void);
    fi(b, PREDICATE, "boolean test(def)", invoke_bool);
    b.m(PREDICATE, "Predicate and(Predicate)", pred_and);
    b.s(PREDICATE, "Predicate isEqual(def)", is_equal);
    b.m(PREDICATE, "Predicate negate()", negate);
    b.m(PREDICATE, "Predicate or(Predicate)", pred_or);
    fi(b, SUPPLIER, "def get()", invoke_def);
    fi(b, TO_DOUBLE_BI_FUNCTION, "double applyAsDouble(def,def)", invoke_double);
    fi(b, TO_DOUBLE_FUNCTION, "double applyAsDouble(def)", invoke_double);
    fi(b, TO_INT_BI_FUNCTION, "int applyAsInt(def,def)", invoke_int);
    fi(b, TO_INT_FUNCTION, "int applyAsInt(def)", invoke_int);
    fi(b, TO_LONG_BI_FUNCTION, "long applyAsLong(def,def)", invoke_long);
    fi(b, TO_LONG_FUNCTION, "long applyAsLong(def)", invoke_long);
    b.interface(UNARY_OPERATOR, &[FUNCTION]);
    b.s(UNARY_OPERATOR, "UnaryOperator identity()", identity);
    b.supers(LAMBDA, &[OBJECT]);

    // java.util.Comparator
    fi(b, COMPARATOR, "int compare(def,def)", invoke_int);
    b.s(COMPARATOR, "Comparator comparing(Function)", comparing);
    b.s(COMPARATOR, "Comparator comparing(Function,Comparator)", comparing2);
    b.s(COMPARATOR, "Comparator comparingDouble(ToDoubleFunction)", comparing_double);
    b.s(COMPARATOR, "Comparator comparingInt(ToIntFunction)", comparing_int);
    b.s(COMPARATOR, "Comparator comparingLong(ToLongFunction)", comparing_long);
    b.m(COMPARATOR, "boolean equals(Object)", fn_equals);
    b.s(COMPARATOR, "Comparator naturalOrder()", natural_order);
    b.s(COMPARATOR, "Comparator nullsFirst(Comparator)", nulls_first);
    b.s(COMPARATOR, "Comparator nullsLast(Comparator)", nulls_last);
    b.m(COMPARATOR, "Comparator reversed()", reversed);
    b.s(COMPARATOR, "Comparator reverseOrder()", reverse_order);
    b.m(COMPARATOR, "Comparator thenComparing(Comparator)", then_comparing);
    b.m(COMPARATOR, "Comparator thenComparing(Function,Comparator)", then_comparing2);
    b.m(COMPARATOR, "Comparator thenComparingDouble(ToDoubleFunction)", then_comparing_double);
    b.m(COMPARATOR, "Comparator thenComparingInt(ToIntFunction)", then_comparing_int);
    b.m(COMPARATOR, "Comparator thenComparingLong(ToLongFunction)", then_comparing_long);
}

#[allow(dead_code)]
fn unused(_: &JArray) {}
