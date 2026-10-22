//! Dynamic (`def`) member access and Java array operations.
//!
//! A `def` call resolves the method by the receiver's runtime class and arity (cached per call
//! site); a `def` field read tries the allowlist first (fields, `getX()`/`isX()` getters — so
//! `map.empty` is `isEmpty()`, as in Elasticsearch), then array `length`, map keys and list
//! indexes. Failures produce Elasticsearch's messages (`dynamic method [java.lang.String,
//! foo/0] not found`, `cannot access method/field [x] from a null def reference`, …).

use std::rc::Rc;

use crate::lookup::{class_name, java_binary_name, MethodId, Registry};
use crate::types::{cls, ClassId, Ty};
use crate::value::{ArrayData, JArray, Obj, Value};
use crate::vm::{iae, npe, Throw, Thrown, Vm, VR};

/// A Java-style method descriptor for NPE messages: `java.util.Map.get(Object)`.
pub fn method_descriptor(reg: &Registry, m: MethodId) -> String {
    let mi = reg.method(m);
    let owner = short_java(java_binary_name(mi.owner));
    let params: Vec<String> = mi.params.iter().map(|t| param_java_name(*t)).collect();
    format!("{owner}.{}({})", mi.name, params.join(", "))
}

/// The JVM abbreviates exactly two classes in helpful NPE messages.
fn short_java(name: &str) -> String {
    match name {
        "java.lang.Object" => "Object".into(),
        "java.lang.String" => "String".into(),
        n => n.to_string(),
    }
}

fn param_java_name(t: Ty) -> String {
    let base = if t.is_primitive() {
        crate::lookup::class_name(t.class).to_string()
    } else if t.class == cls::DEF {
        "Object".to_string()
    } else {
        short_java(java_binary_name(t.class))
    };
    let mut s = base;
    for _ in 0..t.dims {
        s.push_str("[]");
    }
    s
}

/// NPE for a typed call on a null receiver.
pub fn null_receiver(reg: &Registry, m: MethodId, desc: Option<&str>) -> Thrown {
    let mi = reg.method(m);
    if let Some(inner) = &mi.aug {
        return npe(format!("Cannot invoke \"{inner}\" because \"receiver\" is null"));
    }
    let d = method_descriptor(reg, m);
    match desc {
        Some(why) => npe(format!("Cannot invoke \"{d}\" because {why} is null")),
        None => npe(format!("Cannot invoke \"{d}\"")),
    }
}

fn null_def(name: &str) -> Thrown {
    npe(format!("cannot access method/field [{name}] from a null def reference"))
}

/// The runtime class used for member lookup (arrays look up like `Object`).
fn lookup_class(v: &Value) -> ClassId {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(_) => cls::OBJECT,
            _ => o.class(),
        },
        _ => v.class(),
    }
}

/// A method call on a `def` receiver: `args[0]` is the receiver.
pub fn call(vm: &mut Vm, name: u32, site: u32, args: &mut [Value]) -> VR {
    let prog = vm.prog;
    let name_s: &str = &prog.names[name as usize];
    let recv = &args[0];
    if recv.is_null() {
        return Err(null_def(name_s));
    }
    if let Value::Obj(o) = recv
        && let Obj::Host(h) = &**o {
            let rest: Vec<Value> = args[1..].to_vec();
            return h.call(name_s, &rest);
        }
    let class = lookup_class(recv);
    let arity = args.len() - 1;
    let reg = vm.reg;
    let m = match vm.st.dyn_cache.get(site as usize) {
        Some(&(c, m)) if c == class && m != u32::MAX => m,
        _ => {
            let Some(m) = reg.lookup_method(class, false, name_s, arity) else {
                let cname = crate::lookup::type_name(recv.runtime_ty());
                return Err(iae(format!("dynamic method [{cname}, {name_s}/{arity}] not found")));
            };
            if let Some(slot) = vm.st.dyn_cache.get_mut(site as usize) {
                *slot = (class, m);
            }
            m
        }
    };
    let mi = reg.method(m);
    if let Some((arg_tys, ret)) = prog.dyn_sigs.get(site as usize) {
        check_site_type(reg, mi, arg_tys, *ret)?;
    }
    for (i, pt) in mi.params.iter().enumerate() {
        crate::func::link_ref(vm, &args[i + 1], *pt)?;
        let a = std::mem::take(&mut args[i + 1]);
        args[i + 1] = runtime_arg(vm, a, *pt)?;
    }
    vm.call_native(mi.imp, m, args)
}

/// Whether `t` is a numeric box, which runtime (`def`) method lookups bridge: Painless
/// generates a "filtered" method taking `Object` there and converting with
/// `Def.defTo<Box>Implicit` (`PainlessLookupBuilder.generateRuntimeMethods`).
fn bridged_box(t: Ty) -> bool {
    t.dims == 0
        && matches!(t.class, cls::BYTE_BOX | cls::SHORT_BOX | cls::CHARACTER | cls::INTEGER | cls::LONG_BOX | cls::FLOAT_BOX | cls::DOUBLE_BOX)
}

/// An argument of a runtime-resolved method: numeric boxes go through the bridge's
/// `Def.defTo<Box>Implicit` (widening only; `castException` reports "explicitly"), anything
/// else converts as `MethodHandle.asType` does.
fn runtime_arg(vm: &mut Vm, v: Value, t: Ty) -> VR {
    if !bridged_box(t) || v.is_null() {
        return arg_to(vm, v, t);
    }
    use Value as V;
    let ok = match (t.class, &v) {
        (cls::BYTE_BOX, V::Byte(_)) => Some(v.clone()),
        (cls::SHORT_BOX, V::Byte(_) | V::Short(_)) => Some(crate::cast::convert(&v, crate::ir::Prim::Short)),
        (cls::CHARACTER, V::Char(_)) => Some(v.clone()),
        (cls::INTEGER, V::Byte(_) | V::Short(_) | V::Char(_) | V::Int(_)) => Some(crate::cast::convert(&v, crate::ir::Prim::Int)),
        (cls::LONG_BOX, V::Byte(_) | V::Short(_) | V::Char(_) | V::Int(_) | V::Long(_)) => Some(crate::cast::convert(&v, crate::ir::Prim::Long)),
        (cls::FLOAT_BOX, V::Byte(_) | V::Short(_) | V::Char(_) | V::Int(_) | V::Long(_) | V::Float(_)) => Some(crate::cast::convert(&v, crate::ir::Prim::Float)),
        (cls::DOUBLE_BOX, V::Byte(_) | V::Short(_) | V::Char(_) | V::Int(_) | V::Long(_) | V::Float(_) | V::Double(_)) => {
            Some(crate::cast::convert(&v, crate::ir::Prim::Double))
        }
        _ => None,
    };
    match ok {
        Some(x) => Ok(x),
        None => Err(crate::vm::cce(format!("cannot explicitly cast def [{}] to {}", crate::cast::unboxed_name(&v), java_binary_name(t.class)))),
    }
}

/// Whether `MethodHandle.asType` can convert `from` to `to` (`MethodType.canConvert`): primitives
/// widen, box to supertypes of their wrapper, unbox from their wrapper (or a supertype of it,
/// checked at run time); references convert to any reference (cast at run time).
pub fn can_convert(reg: &Registry, from: Ty, to: Ty) -> bool {
    if from == to || from == Ty::DEF || to == Ty::DEF || to == Ty::VOID {
        return true;
    }
    use crate::ir::Prim;
    let wrapper = |p: Prim| match p {
        Prim::Bool => cls::BOOLEAN_BOX,
        Prim::Byte => cls::BYTE_BOX,
        Prim::Short => cls::SHORT_BOX,
        Prim::Char => cls::CHARACTER,
        Prim::Int => cls::INTEGER,
        Prim::Long => cls::LONG_BOX,
        Prim::Float => cls::FLOAT_BOX,
        Prim::Double => cls::DOUBLE_BOX,
    };
    match (from.is_primitive(), to.is_primitive()) {
        (true, true) => match (Prim::of(from), Prim::of(to)) {
            (Some(a), Some(b)) => a == b || (a != Prim::Bool && b != Prim::Bool && crate::cast::widens(a, b)),
            _ => false,
        },
        (true, false) => Prim::of(from).is_some_and(|p| reg.is_assignable(to, Ty::of(wrapper(p)))),
        (false, true) => {
            let Some(p) = Prim::of(to) else { return false };
            // a wrapper unboxes and widens; a supertype of every wrapper is checked at run time
            let unboxes = [Prim::Bool, Prim::Byte, Prim::Short, Prim::Char, Prim::Int, Prim::Long, Prim::Float, Prim::Double]
                .into_iter()
                .any(|w| from == Ty::of(wrapper(w)) && (w == p || (w != Prim::Bool && p != Prim::Bool && crate::cast::widens(w, p))));
            unboxes || (from.dims == 0 && reg.is_assignable(from, Ty::of(wrapper(p))))
        }
        (false, false) => true,
    }
}

fn simple_type_name(t: Ty) -> String {
    let full = if t == Ty::DEF { "java.lang.Object".to_string() } else { crate::lookup::type_name(t) };
    let mut s = full.rsplit('.').next().unwrap_or(&full).to_string();
    if let Some(i) = s.rfind('$') {
        s = s[i + 1..].to_string();
    }
    s
}

/// `WrongMethodTypeException` when a `def` call site's static types cannot be converted to the
/// method found at run time.
fn check_site_type(reg: &Registry, mi: &crate::lookup::MethodInfo, arg_tys: &[Ty], ret: Ty) -> Result<(), Thrown> {
    // the bridge of a method with numeric box parameters takes `Object` there
    let param = |p: Ty| if bridged_box(p) { Ty::OBJECT } else { p };
    let ok = mi.params.iter().zip(arg_tys).all(|(p, a)| can_convert(reg, *a, param(*p))) && can_convert(reg, mi.ret, ret);
    if ok {
        return Ok(());
    }
    let names = |ts: &[Ty]| ts.iter().map(|t| simple_type_name(*t)).collect::<Vec<_>>().join(",");
    let params: Vec<Ty> = mi.params.iter().map(|p| param(*p)).collect();
    let handle = format!("({},{}){}", simple_type_name(Ty::of(mi.owner)), names(&params), simple_type_name(mi.ret));
    let site = format!("(Object,{}){}", names(arg_tys), simple_type_name(ret));
    let handle = handle.replace(",)", ")");
    let site = site.replace(",)", ")");
    Err(Throw::new(cls::WRONG_METHOD_TYPE_EXCEPTION, Some(format!("cannot convert MethodHandle{handle} to {site}"))))
}

/// Converts an argument of a dynamic call to the parameter type (`asType` from the argument's
/// type: unboxing with widening, reference checks).
pub fn arg_to(vm: &mut Vm, v: Value, t: Ty) -> VR {
    if t == Ty::DEF || t == Ty::OBJECT {
        return Ok(v);
    }
    if t.is_primitive() {
        let to = crate::ir::Prim::of(t).unwrap();
        return match crate::cast::prim_of(&v) {
            Some(from) if crate::cast::widens(from, to) => Ok(crate::cast::convert(&v, to)),
            Some(from) => Err(crate::vm::cce(format!("Cannot cast {} to {}", from.box_name(), to.box_name()))
                .with_frames(&["java.base/java.lang.Class.cast(Class.java:3495)"])),
            None if v.is_null() => Err(npe("Cannot unbox a null value")),
            None => Err(crate::vm::cce(format!("Cannot cast {} to {}", java_binary_name(v.class()), to.box_name()))
                .with_frames(&["java.base/java.lang.Class.cast(Class.java:3495)"])),
        };
    }
    if v.is_null() || vm.reg.instance_of(&v, t) {
        return Ok(v);
    }
    // functions accept any function value: the interface is bound when it is called
    if vm.reg.functional(t).is_some()
        && let Value::Obj(o) = &v
            && matches!(&**o, Obj::Function(_)) {
                return Ok(v);
            }
    crate::cast::check(vm, v, t)
}

fn getter_of(reg: &Registry, class: ClassId, name: &str) -> Option<Getter> {
    if let Some(f) = reg.lookup_field(class, false, name) {
        return Some(Getter::Field(f));
    }
    if name.is_empty() {
        return None;
    }
    let mut cap = String::with_capacity(name.len() + 3);
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    cap.extend(first.to_uppercase());
    cap.push_str(chars.as_str());
    if let Some(m) = reg.lookup_method(class, false, &format!("get{cap}"), 0) {
        return Some(Getter::Method(m));
    }
    if let Some(m) = reg.lookup_method(class, false, &format!("is{cap}"), 0) {
        return Some(Getter::Method(m));
    }
    None
}

enum Getter {
    Field(u32),
    Method(MethodId),
}

/// `x.name` on a `def` receiver.
pub fn get_dot(vm: &mut Vm, recv: &Value, name: u32) -> VR {
    let prog = vm.prog;
    let n: &str = &prog.names[name as usize];
    get_dot_str(vm, recv, n)
}

pub fn get_dot_str(vm: &mut Vm, recv: &Value, n: &str) -> VR {
    if recv.is_null() {
        return Err(null_def(n));
    }
    if let Value::Obj(o) = recv
        && let Obj::Host(h) = &**o {
            return h.get(n);
        }
    let reg = vm.reg;
    let class = lookup_class(recv);
    match getter_of(reg, class, n) {
        Some(Getter::Field(f)) => {
            let get = reg.field(f).get;
            return get(vm, &mut [recv.clone()]);
        }
        Some(Getter::Method(m)) => {
            let imp = reg.method(m).imp;
            return imp(vm, &mut [recv.clone()]);
        }
        None => {}
    }
    if let Value::Obj(o) = recv
        && let Obj::Array(a) = &**o
            && n == "length" {
                return Ok(Value::Int(a.len() as i32));
            }
    if is_map(reg, recv) {
        return crate::lib_coll::map_get(vm, recv, &Value::str(n));
    }
    if is_list(reg, recv) {
        let Ok(i) = n.parse::<i32>() else {
            return Err(iae(format!("Illegal list shortcut value [{n}].")));
        };
        return crate::lib_coll::list_get(vm, recv, i);
    }
    Err(iae(format!("dynamic getter [{}, {n}] not found", crate::lookup::type_name(recv.runtime_ty()))))
}

/// `x.name = v` on a `def` receiver.
pub fn set_dot(vm: &mut Vm, recv: &Value, name: u32, v: Value) -> Result<(), Thrown> {
    let prog = vm.prog;
    let n: &str = &prog.names[name as usize];
    set_dot_str(vm, recv, n, v)
}

pub fn set_dot_str(vm: &mut Vm, recv: &Value, n: &str, v: Value) -> Result<(), Thrown> {
    if recv.is_null() {
        return Err(null_def(n));
    }
    if let Value::Obj(o) = recv
        && let Obj::Host(h) = &**o {
            return h.set(n, v);
        }
    let reg = vm.reg;
    let class = lookup_class(recv);
    if !n.is_empty() {
        let mut cap = String::new();
        let mut chars = n.chars();
        cap.extend(chars.next().unwrap().to_uppercase());
        cap.push_str(chars.as_str());
        if let Some(m) = reg.lookup_method(class, false, &format!("set{cap}"), 1) {
            let mi = reg.method(m);
            let pt = mi.params[0];
            let imp = mi.imp;
            let v = arg_to(vm, v, pt)?;
            imp(vm, &mut [recv.clone(), v])?;
            return Ok(());
        }
    }
    if is_map(reg, recv) {
        crate::lib_coll::map_put(vm, recv, Value::str(n), v)?;
        return Ok(());
    }
    if is_list(reg, recv) {
        let Ok(i) = n.parse::<i32>() else {
            return Err(iae(format!("Illegal list shortcut value [{n}].")));
        };
        crate::lib_coll::list_set(vm, recv, i, v)?;
        return Ok(());
    }
    Err(iae(format!("dynamic setter [{}, {n}] not found", crate::lookup::type_name(recv.runtime_ty()))))
}

pub fn is_map(reg: &Registry, v: &Value) -> bool {
    match v {
        Value::Map(_) => true,
        Value::Obj(o) => match &**o {
            Obj::Es(e) => e.is_map(),
            Obj::Host(_) => false,
            _ => reg.is_assignable(Ty::of(cls::MAP), Ty::of(o.class())),
        },
        _ => false,
    }
}

pub fn is_list(reg: &Registry, v: &Value) -> bool {
    match v {
        Value::List(_) => true,
        Value::Obj(o) => match &**o {
            Obj::Es(e) => e.is_list(),
            _ => reg.is_assignable(Ty::of(cls::LIST), Ty::of(o.class())),
        },
        _ => false,
    }
}

/// Linking a `def` index call site whose index type `t` cannot become an `int`: the array and
/// list index normalizers take one, so `asType` fails (maps take any key).
pub fn check_index_site(vm: &mut Vm, recv: &Value, t: Ty) -> Result<(), Thrown> {
    let handle = match recv {
        Value::Obj(o) if matches!(&**o, Obj::Array(_)) => match &**o {
            Obj::Array(a) => {
                let et = if a.elem.dims == 0 && a.elem.is_primitive() { a.elem } else { Ty::OBJECT };
                format!("{}[]", simple_type_name(et))
            }
            _ => unreachable!(),
        },
        v if !v.is_null() && !is_map(vm.reg, v) && is_list(vm.reg, v) => "List".to_string(),
        _ => return Ok(()),
    };
    let n = simple_type_name(t);
    Err(Throw::new(cls::WRONG_METHOD_TYPE_EXCEPTION, Some(format!("cannot convert MethodHandle({handle},int)int to (Object,{n}){n}"))))
}

/// `Def.normalizeIndex`: negative indexes of arrays and lists count from the end.
pub fn normalize_def(vm: &mut Vm, recv: &Value, idx: &Value) -> Result<Value, Thrown> {
    if recv.is_null() {
        // fails while linking the call site, before the brace writes its line number: the
        // position is the previous line number written (none at the start of a function)
        return Err(npe("cannot access method/field [normalizeIndex] from a null def reference"));
    }
    let reg = vm.reg;
    if let Value::Obj(o) = recv
        && let Obj::Array(a) = &**o {
            let i = index_int(idx)?;
            return Ok(Value::Int(if i < 0 { a.len() as i32 + i } else { i }));
        }
    if is_map(reg, recv) {
        return Ok(idx.clone());
    }
    if is_list(reg, recv) {
        let i = index_int(idx)?;
        if i < 0 {
            let n = crate::lib_coll::size_of(vm, recv)?;
            return Ok(Value::Int(n + i));
        }
        return Ok(Value::Int(i));
    }
    Err(iae(format!(
        "Attempting to address a non-array-like type [{}] as an array.",
        crate::lookup::type_name(recv.runtime_ty())
    )))
}

fn index_int(idx: &Value) -> Result<i32, Thrown> {
    match idx {
        Value::Int(i) => Ok(*i),
        Value::Byte(_) | Value::Short(_) | Value::Char(_) => Ok(idx.as_i64().unwrap() as i32),
        Value::Null => Err(npe("Cannot unbox a null value")),
        other => Err(crate::vm::cce(format!("Cannot cast {} to java.lang.Integer", java_binary_name(other.class())))
            .with_frames(&["java.base/java.lang.Class.cast(Class.java:3495)"])),
    }
}

/// `x[i]` on a `def` receiver, with `idx` already normalized.
pub fn get_index(vm: &mut Vm, recv: &Value, idx: &Value) -> VR {
    let i = idx.clone();
    if let Value::Obj(o) = recv
        && let Obj::Array(_) = &**o {
            return array_load(recv, &i, None);
        }
    let reg = vm.reg;
    if is_map(reg, recv) {
        return crate::lib_coll::map_get(vm, recv, &i);
    }
    let i = index_int(&i)?;
    if let Value::List(l) = recv {
        crate::lib_coll::view_enter(l, &crate::lib_coll::ViewCall { owner: cls::LIST, name: "get", arity: 1 })?;
    }
    crate::lib_coll::list_get(vm, recv, i)
}

/// `x[i] = v` on a `def` receiver, with `idx` already normalized. `vty` is the static type
/// of the value when it is primitive: the call site's `asType` cannot narrow it.
pub fn set_index(vm: &mut Vm, recv: &Value, idx: &Value, v: Value, vty: Option<Ty>) -> Result<(), Thrown> {
    let i = idx.clone();
    if let Value::Obj(o) = recv
        && let Obj::Array(a) = &**o {
            if let (Some(st), true) = (vty, a.elem.is_primitive()) {
                let (from, to) = (crate::ir::Prim::of(st), crate::ir::Prim::of(a.elem));
                if let (Some(f), Some(t)) = (from, to)
                    && !crate::cast::widens(f, t) {
                        let at = crate::lookup::type_name(a.ty());
                        let et = crate::lookup::type_name(a.elem);
                        let vt = crate::lookup::type_name(st);
                        let mut t = Throw::new(cls::RUNTIME_EXCEPTION, Some(format!("cannot convert MethodHandle({at},int,{et})void to (Object,int,{vt})void")));
                        t.es_type = Some("wrong_method_type_exception");
                        return Err(t);
                    }
            }
            let v = arg_to(vm, v, a.elem)?;
            return array_store(recv, &i, v, None);
        }
    let reg = vm.reg;
    if is_map(reg, recv) {
        crate::lib_coll::map_put(vm, recv, i, v)?;
        return Ok(());
    }
    let i = index_int(&i)?;
    if let Value::List(l) = recv {
        crate::lib_coll::view_enter(l, &crate::lib_coll::ViewCall { owner: cls::LIST, name: "set", arity: 2 })?;
        crate::lib_coll::list_set(vm, recv, i, v)?;
        crate::lib_coll::view_exit(l);
        return Ok(());
    }
    crate::lib_coll::list_set(vm, recv, i, v)?;
    Ok(())
}

fn array_kind(a: &JArray) -> &'static str {
    if a.elem.dims > 0 {
        return "object";
    }
    match a.elem.class {
        cls::INT => "int",
        cls::LONG => "long",
        cls::DOUBLE => "double",
        cls::FLOAT => "float",
        cls::CHAR => "char",
        cls::SHORT => "short",
        cls::BYTE | cls::BOOLEAN => "byte/boolean",
        _ => "object",
    }
}

fn because(desc: Option<&str>) -> String {
    desc.map(|d| format!(" because {d} is null")).unwrap_or_default()
}

fn aioobe(i: i32, len: usize) -> Thrown {
    Throw::new(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {i} out of bounds for length {len}")))
}

fn as_array(v: &Value) -> Option<&JArray> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(a) => Some(a),
            _ => None,
        },
        _ => None,
    }
}

pub fn array_load(arr: &Value, idx: &Value, desc: Option<&str>) -> VR {
    let i = idx.as_i64().unwrap_or(0) as i32;
    let Some(a) = as_array(arr) else {
        if arr.is_null() {
            return Err(npe(format!("Cannot load from object array{}", because(desc))));
        }
        return Err(crate::vm::cce("not an array"));
    };
    let d = a.data.borrow();
    if i < 0 || i as usize >= d.len() {
        return Err(aioobe(i, d.len()));
    }
    Ok(d.get(i as usize))
}

pub fn array_store(arr: &Value, idx: &Value, v: Value, desc: Option<&str>) -> Result<(), Thrown> {
    let i = idx.as_i64().unwrap_or(0) as i32;
    let Some(a) = as_array(arr) else {
        if arr.is_null() {
            return Err(npe(format!("Cannot store to object array{}", because(desc))));
        }
        return Err(crate::vm::cce("not an array"));
    };
    let mut d = a.data.borrow_mut();
    if i < 0 || i as usize >= d.len() {
        return Err(aioobe(i, d.len()));
    }
    if let ArrayData::Ref(_) = &*d
        && !v.is_null() && a.elem != Ty::DEF && a.elem != Ty::OBJECT && !crate::lookup::registry().instance_of(&v, a.elem) {
            return Err(Throw::new(cls::ARRAY_STORE_EXCEPTION, Some(java_binary_name(v.class()).to_string())));
        }
    d.set(i as usize, v);
    Ok(())
}

/// NPE text for typed array access on a null array (the instruction knows the array type).
pub fn null_array(kind_of: Ty, load: bool, desc: Option<&str>) -> Thrown {
    let k = JArray::new(kind_of.component(), 0);
    let kind = array_kind(&k);
    if load {
        npe(format!("Cannot load from {kind} array{}", because(desc)))
    } else {
        npe(format!("Cannot store to {kind} array{}", because(desc)))
    }
}

pub fn array_len(arr: &Value, desc: Option<&str>) -> Result<i32, Thrown> {
    match as_array(arr) {
        Some(a) => Ok(a.len() as i32),
        None => Err(npe(format!("Cannot read the array length{}", because(desc)))),
    }
}

/// Negative index normalization for brace access on arrays and lists.
pub fn flip(recv: &Value, idx: &Value, list: bool, desc: Option<&str>) -> Result<i32, Thrown> {
    let i = idx.as_i64().unwrap_or(0) as i32;
    if i >= 0 {
        return Ok(i);
    }
    if list {
        if recv.is_null() {
            return Err(npe(format!("Cannot invoke \"java.util.Collection.size()\"{}", because(desc))));
        }
        let n = match recv {
            Value::List(l) => l.len() as i32,
            other => other.list_snapshot().map(|v| v.len() as i32).unwrap_or(0),
        };
        return Ok(n + i);
    }
    Ok(array_len(recv, desc)? + i)
}

/// `new T[a][b]…`: `t` is the array type.
pub fn new_array(t: Ty, dims: &[i64]) -> VR {
    for &d in dims {
        if d < 0 {
            return Err(Throw::new(cls::NEGATIVE_ARRAY_SIZE_EXCEPTION, Some(d.to_string())));
        }
    }
    // every level of a multi-dimensional array is allocated up front: check the whole of it
    // (arrays at this level, their elements in all)
    let mut arrays: u64 = 1;
    let mut total: u64 = 0;
    let mut elem = t;
    for (i, &d) in dims.iter().enumerate() {
        elem = elem.component();
        let last = i + 1 == dims.len();
        let width = if last { crate::value::elem_bytes(elem) } else { std::mem::size_of::<Value>() as u64 };
        crate::guard::check_array(d as u64, width)?;
        let elements = arrays.saturating_mul(d as u64);
        total = total.saturating_add(arrays.saturating_mul(16)).saturating_add(elements.saturating_mul(width));
        crate::guard::check_bytes(total)?;
        arrays = elements;
        if arrays == 0 {
            break;
        }
    }
    Ok(make_array(t, dims))
}

fn make_array(t: Ty, dims: &[i64]) -> Value {
    let elem = t.component();
    let n = dims[0] as usize;
    let a = JArray::new(elem, n);
    if dims.len() > 1
        && let ArrayData::Ref(v) = &mut *a.data.borrow_mut() {
            for slot in v.iter_mut() {
                *slot = make_array(elem, &dims[1..]);
            }
        }
    Value::Obj(Rc::new(Obj::Array(a)))
}

/// `new T[] {…}` from already converted values.
pub fn array_from(t: Ty, items: Vec<Value>) -> Value {
    let a = JArray::new(t.component(), items.len());
    {
        let mut d = a.data.borrow_mut();
        for (i, v) in items.into_iter().enumerate() {
            d.set(i, v);
        }
    }
    Value::Obj(Rc::new(Obj::Array(a)))
}


#[allow(dead_code)]
fn owner_name(reg: &Registry, m: MethodId) -> &'static str {
    class_name(reg.method(m).owner)
}

/// Calls a method by name on a value's runtime class (library code acting like a `def` call).
pub fn call_named(vm: &mut Vm, recv: &Value, name: &str, args: Vec<Value>) -> VR {
    if recv.is_null() {
        return Err(null_def(name));
    }
    let reg = vm.reg;
    let class = lookup_class(recv);
    let Some(m) = reg.lookup_method(class, false, name, args.len()) else {
        let cname = crate::lookup::type_name(recv.runtime_ty());
        return Err(iae(format!("dynamic method [{cname}, {name}/{}] not found", args.len())));
    };
    let mi = reg.method(m);
    let mut full = Vec::with_capacity(args.len() + 1);
    full.push(recv.clone());
    for (i, a) in args.into_iter().enumerate() {
        full.push(runtime_arg(vm, a, mi.params[i])?);
    }
    vm.call_native(mi.imp, m, &mut full)
}
