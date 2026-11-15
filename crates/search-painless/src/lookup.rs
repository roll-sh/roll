//! The allowlist: every class scripts can see, with its supertypes, methods, constructors and
//! fields, and the native implementation behind each member.
//!
//! Painless resolves members by name *and arity* (there is no overloading by type), so a class
//! keeps `name/arity → method` maps. Instance methods are inherited — a class's map holds its
//! own methods first and then those of its superclass and interfaces, breadth first, which is
//! also the order the runtime walks for `def` receivers. Static methods and constructors are
//! not inherited.
//!
//! The registry is built once per process ([`registry`]) from the library modules
//! (`lib_lang`, `lib_coll`, …), each of which declares members with signature strings such as
//! `"def get(int)"`; types in signatures resolve through the same names scripts use.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::types::{cls, ClassId, Ty};
use crate::value::Value;
use crate::vm::{Thrown, Vm};

pub type MethodId = u32;

/// A library method: receives the receiver (for instance methods) followed by the arguments,
/// already converted to the declared parameter types.
pub type Native = for<'a, 'b> fn(&'b mut Vm<'a>, &mut [Value]) -> Result<Value, Thrown>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MethodKind {
    Instance,
    Static,
    Ctor,
}

pub struct MethodInfo {
    pub name: Box<str>,
    /// Class the member is declared on (what Elasticsearch's lookup calls the target class).
    pub owner: ClassId,
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub kind: MethodKind,
    pub imp: Native,
    /// Calls with this flag record that the script is not deterministic (cache hints).
    pub nondeterministic: bool,
    /// For Painless augmentations (static helpers taking the receiver): the Java method the
    /// helper calls on its receiver, which is what a null receiver's NPE names.
    pub aug: Option<Box<str>>,
}

pub struct FieldInfo {
    pub name: Box<str>,
    pub ty: Ty,
    pub is_static: bool,
    pub is_final: bool,
    /// For static fields: produces the value (from `[data]`). For instance fields: reads it from
    /// args[0].
    pub get: Native,
    /// Passed to a static field's `get` (one native serves a family of constants).
    pub data: i64,
}

pub struct ClassInfo {
    pub id: ClassId,
    pub name: Box<str>,
    pub short: Option<Box<str>>,
    pub supers: Vec<ClassId>,
    /// All supertypes including the class itself, breadth first.
    pub ancestors: Vec<ClassId>,
    pub own: Vec<MethodId>,
    pub methods: HashMap<(Box<str>, u8), MethodId>,
    pub statics: HashMap<(Box<str>, u8), MethodId>,
    pub ctors: HashMap<u8, MethodId>,
    pub fields: HashMap<Box<str>, u32>,
    pub static_fields: HashMap<Box<str>, u32>,
    /// The single abstract method of a functional interface.
    pub functional: Option<MethodId>,
    pub interface: bool,
    /// Classes Elasticsearch marks `@dynamic_type`: unknown methods dispatch dynamically.
    pub dynamic_type: bool,
}

pub struct Registry {
    pub classes: Vec<ClassInfo>,
    pub methods: Vec<MethodInfo>,
    pub fields: Vec<FieldInfo>,
    /// Canonical and imported names → class.
    pub names: HashMap<Box<str>, ClassId>,
    /// Imported static methods (`static_import` in the allowlists) by name/arity, per script
    /// context family; see [`crate::contexts`].
    pub imports: HashMap<(Box<str>, u8), MethodId>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(build)
}

fn build() -> Registry {
    let mut b = Builder::new();
    crate::lib_lang::register(&mut b);
    crate::unicode::register(&mut b);
    crate::charblocks::register(&mut b);
    crate::lib_coll::register(&mut b);
    crate::lib_util::register(&mut b);
    crate::func::register(&mut b);
    crate::stream::register(&mut b);
    crate::regex::register(&mut b);
    crate::time::register(&mut b);
    crate::bignum::register(&mut b);
    crate::es::register(&mut b);
    crate::text::register(&mut b);
    b.finish()
}

/// Canonical type name (`PainlessLookupUtility.typeToCanonicalTypeName`).
pub fn type_name(t: Ty) -> String {
    let base = class_name(t.class);
    let mut s = String::with_capacity(base.len() + 2 * t.dims as usize);
    s.push_str(base);
    for _ in 0..t.dims {
        s.push_str("[]");
    }
    s
}

pub fn class_name(c: ClassId) -> &'static str {
    let r = registry();
    match r.classes.get(c.0 as usize) {
        Some(ci) => &ci.name,
        None => "def",
    }
}

/// `Class.getName()` style name (nested classes joined with `$`) for messages produced by the
/// JVM itself, such as `ClassCastException` texts and array descriptors.
pub fn java_binary_name(c: ClassId) -> &'static str {
    static BIN: OnceLock<Vec<String>> = OnceLock::new();
    let names = BIN.get_or_init(|| {
        let r = registry();
        r.classes
            .iter()
            .map(|ci| {
                // packages are lower case, nested classes start upper case
                let parts: Vec<&str> = ci.name.split('.').collect();
                let mut out = String::new();
                let mut in_class = false;
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.push(if in_class { '$' } else { '.' });
                    }
                    out.push_str(p);
                    if p.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                        in_class = true;
                    }
                }
                out
            })
            .collect()
    });
    names.get(c.0 as usize).map(|s| s.as_str()).unwrap_or("java.lang.Object")
}

impl Registry {
    pub fn class(&self, c: ClassId) -> &ClassInfo {
        &self.classes[c.0 as usize]
    }
    pub fn method(&self, m: MethodId) -> &MethodInfo {
        &self.methods[m as usize]
    }
    pub fn field(&self, f: u32) -> &FieldInfo {
        &self.fields[f as usize]
    }

    /// `canonicalTypeNameToType`: canonical or imported names, `def`, primitives, arrays.
    pub fn resolve_type(&self, name: &str) -> Option<Ty> {
        let mut base = name;
        let mut dims = 0u8;
        while let Some(b) = base.strip_suffix("[]") {
            base = b;
            dims += 1;
        }
        let c = *self.names.get(base)?;
        if c == cls::VOID && dims > 0 {
            return None;
        }
        Some(Ty { class: c, dims })
    }

    /// Whether a symbol names a class (declarations may not use class names as variable names).
    pub fn is_class_name(&self, name: &str) -> bool {
        self.names.contains_key(name)
    }

    pub fn lookup_method(&self, c: ClassId, is_static: bool, name: &str, arity: usize) -> Option<MethodId> {
        // a primitive receiver is boxed for the lookup (`5.compareTo(6)`, `x.hashCode()`)
        let c = Ty::of(c).boxed().class;
        let ci = self.classes.get(c.0 as usize)?;
        let key = (Box::<str>::from(name), arity as u8);
        if is_static {
            ci.statics.get(&key).copied()
        } else {
            ci.methods.get(&key).copied()
        }
    }

    /// Method lookup for a receiver's static type, arrays included (an array is an `Object`).
    pub fn lookup_instance(&self, t: Ty, name: &str, arity: usize) -> Option<MethodId> {
        let c = if t.dims > 0 { cls::OBJECT } else { t.class };
        self.lookup_method(c, false, name, arity)
    }

    pub fn lookup_ctor(&self, c: ClassId, arity: usize) -> Option<MethodId> {
        self.classes.get(c.0 as usize)?.ctors.get(&(arity as u8)).copied()
    }

    pub fn lookup_field(&self, c: ClassId, is_static: bool, name: &str) -> Option<u32> {
        let ci = self.classes.get(c.0 as usize)?;
        if is_static {
            ci.static_fields.get(name).copied()
        } else {
            // instance fields are inherited
            for a in &ci.ancestors {
                if let Some(f) = self.classes[a.0 as usize].fields.get(name) {
                    return Some(*f);
                }
            }
            None
        }
    }

    pub fn lookup_import(&self, name: &str, arity: usize) -> Option<MethodId> {
        self.imports.get(&(Box::<str>::from(name), arity as u8)).copied()
    }

    /// `Class.isAssignableFrom`: can a value of type `from` be used where `to` is expected?
    pub fn is_assignable(&self, to: Ty, from: Ty) -> bool {
        if to == from {
            return true;
        }
        if to.dims == 0 && to.class == cls::OBJECT {
            return !from.is_primitive() && from != Ty::VOID && from != Ty::DEF;
        }
        if to.dims > 0 || from.dims > 0 {
            if to.dims == from.dims {
                // `def` is a class extending `Object` (and extended by nothing), so with array
                // covariance `def[]` fits `Object[]` but no other array fits `def[]`
                if from.class == cls::DEF {
                    return to.class == cls::OBJECT;
                }
                if to.class == cls::DEF {
                    return false;
                }
                let (tb, fb) = (Ty::of(to.class), Ty::of(from.class));
                if tb.is_primitive() || fb.is_primitive() {
                    return tb == fb;
                }
                return self.is_assignable(tb, fb);
            }
            if from.dims > to.dims && to.class == cls::OBJECT {
                return true;
            }
            return false;
        }
        if from.is_primitive() || to.is_primitive() {
            return false;
        }
        match self.classes.get(from.class.0 as usize) {
            Some(ci) => ci.ancestors.contains(&to.class),
            None => false,
        }
    }

    /// Runtime `instanceof` of a value against a type (primitive types test the boxed class).
    pub fn instance_of(&self, v: &Value, t: Ty) -> bool {
        if v.is_null() {
            return false;
        }
        if t == Ty::DEF {
            return true;
        }
        let mut t = t.boxed();
        // at run time `def[]` is `Object[]`
        if t.dims > 0 && t.class == cls::DEF {
            t.class = cls::OBJECT;
        }
        self.is_assignable(t, v.runtime_ty())
    }

    /// The functional interface method of a type, if it is one.
    pub fn functional(&self, t: Ty) -> Option<MethodId> {
        if t.dims > 0 {
            return None;
        }
        self.classes.get(t.class.0 as usize)?.functional
    }
}

// ------------------------------------------------------------------------------------ builder

pub struct Builder {
    classes: Vec<ClassInfo>,
    methods: Vec<MethodInfo>,
    fields: Vec<FieldInfo>,
    names: HashMap<Box<str>, ClassId>,
    imports: HashMap<(Box<str>, u8), MethodId>,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Builder {
        let mut classes = Vec::with_capacity(cls::BUILTIN_COUNT);
        let mut names = HashMap::new();
        for (i, (java, short)) in cls::BUILTIN.iter().enumerate() {
            let id = ClassId(i as u16);
            classes.push(ClassInfo {
                id,
                name: (*java).into(),
                short: short.map(Into::into),
                supers: Vec::new(),
                ancestors: Vec::new(),
                own: Vec::new(),
                methods: HashMap::new(),
                statics: HashMap::new(),
                ctors: HashMap::new(),
                fields: HashMap::new(),
                static_fields: HashMap::new(),
                functional: None,
                interface: false,
                dynamic_type: false,
            });
            // primitives and def are named by their short form only; runtime-only classes have
            // no short form and stay unnamed (scripts cannot refer to them)
            if let Some(s) = short {
                names.insert(Box::<str>::from(*s), id);
                if *s != *java {
                    names.insert(Box::<str>::from(*java), id);
                }
            }
        }
        Builder { classes, methods: Vec::new(), fields: Vec::new(), names, imports: HashMap::new() }
    }

    /// Declares the direct supertypes of a class (superclass first).
    pub fn supers(&mut self, c: ClassId, supers: &[ClassId]) {
        self.classes[c.0 as usize].supers = supers.to_vec();
    }

    pub fn interface(&mut self, c: ClassId, supers: &[ClassId]) {
        self.classes[c.0 as usize].supers = supers.to_vec();
        self.classes[c.0 as usize].interface = true;
    }

    pub fn dynamic_type(&mut self, c: ClassId) {
        self.classes[c.0 as usize].dynamic_type = true;
    }

    /// Registers an additional class at runtime (host extensions).
    pub fn new_class(&mut self, name: &str, short: Option<&str>, supers: &[ClassId]) -> ClassId {
        let id = ClassId(self.classes.len() as u16);
        self.classes.push(ClassInfo {
            id,
            name: name.into(),
            short: short.map(Into::into),
            supers: supers.to_vec(),
            ancestors: Vec::new(),
            own: Vec::new(),
            methods: HashMap::new(),
            statics: HashMap::new(),
            ctors: HashMap::new(),
            fields: HashMap::new(),
            static_fields: HashMap::new(),
            functional: None,
            interface: false,
            dynamic_type: false,
        });
        self.names.insert(name.into(), id);
        if let Some(s) = short {
            self.names.insert(s.into(), id);
        }
        id
    }

    pub fn ty(&self, name: &str) -> Ty {
        let name = name.trim();
        let mut base = name;
        let mut dims = 0u8;
        while let Some(b) = base.strip_suffix("[]") {
            base = b.trim_end();
            dims += 1;
        }
        match self.names.get(base) {
            Some(c) => Ty { class: *c, dims },
            None => panic!("allowlist type [{name}] not found"),
        }
    }

    fn parse_sig(&self, sig: &str) -> (Ty, String, Vec<Ty>) {
        let open = sig.find('(').unwrap_or_else(|| panic!("bad signature {sig}"));
        let close = sig.rfind(')').unwrap_or_else(|| panic!("bad signature {sig}"));
        let head = sig[..open].trim();
        let params: Vec<Ty> = sig[open + 1..close].split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| self.ty(s)).collect();
        if head.is_empty() {
            return (Ty::VOID, String::new(), params);
        }
        let sp = head.rfind(' ').unwrap_or_else(|| panic!("bad signature {sig}"));
        (self.ty(&head[..sp]), head[sp + 1..].to_string(), params)
    }

    fn push_method(&mut self, owner: ClassId, name: &str, params: Vec<Ty>, ret: Ty, kind: MethodKind, imp: Native) -> MethodId {
        let id = self.methods.len() as MethodId;
        self.methods.push(MethodInfo { name: name.into(), owner, params, ret, kind, imp, nondeterministic: false, aug: None });
        id
    }

    /// Instance method, e.g. `b.m(cls::LIST, "def get(int)", list_get)`.
    pub fn m(&mut self, c: ClassId, sig: &str, imp: Native) -> MethodId {
        let (ret, name, params) = self.parse_sig(sig);
        let arity = params.len() as u8;
        let id = self.push_method(c, &name, params, ret, MethodKind::Instance, imp);
        let ci = &mut self.classes[c.0 as usize];
        ci.own.retain(|&m| !(self.methods[m as usize].name.as_ref() == name && self.methods[m as usize].params.len() as u8 == arity));
        ci.own.push(id);
        id
    }

    /// Static method.
    pub fn s(&mut self, c: ClassId, sig: &str, imp: Native) -> MethodId {
        let (ret, name, params) = self.parse_sig(sig);
        let arity = params.len() as u8;
        let id = self.push_method(c, &name, params, ret, MethodKind::Static, imp);
        self.classes[c.0 as usize].statics.insert((name.into(), arity), id);
        id
    }

    /// Constructor, e.g. `b.c(cls::ARRAY_LIST, "(Collection)", …)`.
    pub fn c(&mut self, c: ClassId, sig: &str, imp: Native) -> MethodId {
        let (_, _, params) = self.parse_sig(sig);
        let arity = params.len() as u8;
        let id = self.push_method(c, "<init>", params, Ty::of(c), MethodKind::Ctor, imp);
        self.classes[c.0 as usize].ctors.insert(arity, id);
        id
    }

    /// Marks a method as a Painless augmentation that calls `inner` (a Java method descriptor
    /// such as `java.lang.Iterable.forEach(java.util.function.Consumer)`) on its receiver.
    pub fn aug(&mut self, m: MethodId, inner: &str) {
        self.methods[m as usize].aug = Some(inner.into());
    }

    /// Marks the last-registered method nondeterministic.
    pub fn nondeterministic(&mut self, m: MethodId) {
        self.methods[m as usize].nondeterministic = true;
    }

    /// Static field with a value-producing function.
    pub fn sf(&mut self, c: ClassId, decl: &str, get: Native) {
        let decl = decl.trim();
        let sp = decl.rfind(' ').unwrap();
        let ty = self.ty(&decl[..sp]);
        let name = &decl[sp + 1..];
        let id = self.fields.len() as u32;
        self.fields.push(FieldInfo { name: name.into(), ty, is_static: true, is_final: true, get, data: 0 });
        self.classes[c.0 as usize].static_fields.insert(name.into(), id);
    }

    /// Static constant produced by `get([Long(data)])`.
    pub fn sf_data(&mut self, c: ClassId, decl: &str, get: Native, data: i64) {
        self.sf(c, decl, get);
        if let Some(f) = self.fields.last_mut() {
            f.data = data;
        }
    }

    /// Instance field read through a native.
    pub fn f(&mut self, c: ClassId, decl: &str, get: Native) {
        let decl = decl.trim();
        let sp = decl.rfind(' ').unwrap();
        let ty = self.ty(&decl[..sp]);
        let name = &decl[sp + 1..];
        let id = self.fields.len() as u32;
        self.fields.push(FieldInfo { name: name.into(), ty, is_static: false, is_final: true, get, data: 0 });
        self.classes[c.0 as usize].fields.insert(name.into(), id);
    }

    /// Marks a class as a functional interface with the given abstract method.
    pub fn functional(&mut self, c: ClassId, sig: &str, imp: Native) -> MethodId {
        let id = self.m(c, sig, imp);
        self.classes[c.0 as usize].functional = Some(id);
        self.classes[c.0 as usize].interface = true;
        id
    }

    /// `static_import` of a function (callable without a qualifier).
    pub fn import(&mut self, owner: ClassId, sig: &str, imp: Native) -> MethodId {
        let (ret, name, params) = self.parse_sig(sig);
        let arity = params.len() as u8;
        let id = self.push_method(owner, &name, params, ret, MethodKind::Static, imp);
        self.imports.insert((name.into(), arity), id);
        id
    }

    fn finish(mut self) -> Registry {
        let n = self.classes.len();
        // ancestors, breadth first
        for i in 0..n {
            let mut seen = vec![ClassId(i as u16)];
            let mut q = 0;
            while q < seen.len() {
                let c = seen[q];
                q += 1;
                for &s in &self.classes[c.0 as usize].supers {
                    if !seen.contains(&s) {
                        seen.push(s);
                    }
                }
            }
            // everything (but primitives/void/def) is an Object
            if i >= 10 && !seen.contains(&cls::OBJECT) {
                seen.push(cls::OBJECT);
            }
            self.classes[i].ancestors = seen;
        }
        // inherited method maps
        for i in 0..n {
            let ancestors = self.classes[i].ancestors.clone();
            let mut map: HashMap<(Box<str>, u8), MethodId> = HashMap::new();
            for a in ancestors {
                for &m in &self.classes[a.0 as usize].own {
                    let mi = &self.methods[m as usize];
                    map.entry((mi.name.clone(), mi.params.len() as u8)).or_insert(m);
                }
            }
            // a functional interface inherits its abstract method from a parent interface
            // (UnaryOperator → Function.apply)
            if self.classes[i].functional.is_none() && self.classes[i].interface {
                let anc = self.classes[i].ancestors.clone();
                for a in anc.iter().skip(1) {
                    if let Some(f) = self.classes[a.0 as usize].functional {
                        self.classes[i].functional = Some(f);
                        break;
                    }
                }
            }
            self.classes[i].methods = map;
        }
        Registry { classes: self.classes, methods: self.methods, fields: self.fields, names: self.names, imports: self.imports }
    }
}

/// Every allowlisted member as `class|kind|name/arity` lines (`kind` is `m`, `s`, `c`, `f`
/// or `sf`), for checking the registry against Elasticsearch's allowlist files.
#[doc(hidden)]
pub fn dump() -> Vec<String> {
    let r = registry();
    let mut out = Vec::new();
    for ci in &r.classes {
        let name = &ci.name;
        for k in ci.methods.keys() {
            out.push(format!("{name}|m|{}/{}", k.0, k.1));
        }
        for k in ci.statics.keys() {
            out.push(format!("{name}|s|{}/{}", k.0, k.1));
        }
        for k in ci.ctors.keys() {
            out.push(format!("{name}|c|<init>/{k}"));
        }
        for k in ci.fields.keys() {
            out.push(format!("{name}|f|{k}"));
        }
        for k in ci.static_fields.keys() {
            out.push(format!("{name}|sf|{k}"));
        }
        for a in &ci.ancestors {
            out.push(format!("{name}|a|{}", r.classes[a.0 as usize].name));
        }
    }
    for k in r.imports.keys() {
        out.push(format!("<import>|s|{}/{}", k.0, k.1));
    }
    out.sort();
    out
}
