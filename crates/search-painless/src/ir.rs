//! The typed tree the analyzer produces and the bytecode compiler consumes.
//!
//! Every expression knows its static type and every implicit conversion is an explicit
//! [`Cast`] node, so the compiler never reasons about types: it picks the instruction the
//! analyzer already decided on. Loads and stores of everything addressable share one
//! [`Place`] description (a local, `x.y` through a getter, `map.key`, `list[i]`, an array
//! element, a `def` field…), which keeps compound assignment (`a.b[i] += 1`) a single code path.

use std::sync::Arc;

use crate::ast::Op;
use crate::lookup::MethodId;
use crate::types::{ClassId, Ty};

/// A compile-time constant.
#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Char(u16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Str(Arc<str>),
}

impl Const {
    /// `"" + constant` as Java would print it.
    pub fn to_java_string(&self) -> String {
        match self {
            Const::Bool(b) => b.to_string(),
            Const::Byte(v) => v.to_string(),
            Const::Short(v) => v.to_string(),
            Const::Char(c) => crate::java::char_to_string(*c),
            Const::Int(v) => v.to_string(),
            Const::Long(v) => v.to_string(),
            Const::Float(f) => crate::java::fmt_float(*f),
            Const::Double(d) => crate::java::fmt_double(*d),
            Const::Str(s) => s.to_string(),
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        Some(match *self {
            Const::Byte(v) => v as i64,
            Const::Short(v) => v as i64,
            Const::Char(v) => v as i64,
            Const::Int(v) => v as i64,
            Const::Long(v) => v,
            _ => return None,
        })
    }
    pub fn as_f64(&self) -> Option<f64> {
        Some(match *self {
            Const::Float(v) => v as f64,
            Const::Double(v) => v,
            _ => self.as_i64()? as f64,
        })
    }
}


/// Primitive kinds a value can be converted to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    Bool,
    Byte,
    Short,
    Char,
    Int,
    Long,
    Float,
    Double,
}

impl Prim {
    pub const ALL: [Prim; 8] = [Prim::Bool, Prim::Byte, Prim::Short, Prim::Char, Prim::Int, Prim::Long, Prim::Float, Prim::Double];
    pub fn from_index(i: u8) -> Prim {
        Prim::ALL[i as usize]
    }
    pub fn index(self) -> u8 {
        Prim::ALL.iter().position(|p| *p == self).unwrap() as u8
    }
    pub fn of(t: Ty) -> Option<Prim> {
        if t.dims != 0 {
            return None;
        }
        Some(match t.class {
            crate::types::cls::BOOLEAN | crate::types::cls::BOOLEAN_BOX => Prim::Bool,
            crate::types::cls::BYTE | crate::types::cls::BYTE_BOX => Prim::Byte,
            crate::types::cls::SHORT | crate::types::cls::SHORT_BOX => Prim::Short,
            crate::types::cls::CHAR | crate::types::cls::CHARACTER => Prim::Char,
            crate::types::cls::INT | crate::types::cls::INTEGER => Prim::Int,
            crate::types::cls::LONG | crate::types::cls::LONG_BOX => Prim::Long,
            crate::types::cls::FLOAT | crate::types::cls::FLOAT_BOX => Prim::Float,
            crate::types::cls::DOUBLE | crate::types::cls::DOUBLE_BOX => Prim::Double,
            _ => return None,
        })
    }
    pub fn ty(self) -> Ty {
        match self {
            Prim::Bool => Ty::BOOLEAN,
            Prim::Byte => Ty::BYTE,
            Prim::Short => Ty::SHORT,
            Prim::Char => Ty::CHAR,
            Prim::Int => Ty::INT,
            Prim::Long => Ty::LONG,
            Prim::Float => Ty::FLOAT,
            Prim::Double => Ty::DOUBLE,
        }
    }
    pub fn java_name(self) -> &'static str {
        match self {
            Prim::Bool => "boolean",
            Prim::Byte => "byte",
            Prim::Short => "short",
            Prim::Char => "char",
            Prim::Int => "int",
            Prim::Long => "long",
            Prim::Float => "float",
            Prim::Double => "double",
        }
    }
    pub fn box_name(self) -> &'static str {
        match self {
            Prim::Bool => "java.lang.Boolean",
            Prim::Byte => "java.lang.Byte",
            Prim::Short => "java.lang.Short",
            Prim::Char => "java.lang.Character",
            Prim::Int => "java.lang.Integer",
            Prim::Long => "java.lang.Long",
            Prim::Float => "java.lang.Float",
            Prim::Double => "java.lang.Double",
        }
    }
}

/// A value conversion.
#[derive(Clone, Debug, PartialEq)]
pub enum Cast {
    /// Primitive (or boxed) number/char to another primitive kind; `unbox` names the source's
    /// primitive when the source is a boxed type (unboxing a null throws).
    Num { to: Prim, unbox: Option<Prim> },
    /// Unboxing to the same primitive kind (null check only).
    Unbox(Prim),
    /// `Def.defToXImplicit` / `defToXExplicit`: `def` to a primitive or boxed type.
    DefTo { to: Prim, explicit: bool, boxed: bool },
    /// `def` (or a reference) to `String`: implicit allows only strings, explicit also chars.
    DefToString { explicit: bool },
    /// `MethodHandle.asType` from `Object`: the result of a `def` call, field or index used
    /// directly as a typed value.
    AsType(Ty),
    /// The result of a `def` operator used as a typed value (`DefMath` / `MIC` semantics).
    MathResult { to: Ty, explicit: bool },
    /// `(char) "a"`.
    StringToChar,
    /// `(String) 'a'`.
    CharToString,
    /// Reference downcast (`checkcast`).
    Check(Ty),
    /// A conversion with no runtime effect (boxing, upcasts, to `def`). It still exists because
    /// Elasticsearch emits a cast node there, which moves the line-number table to the
    /// converted expression and so decides where a later runtime error points.
    Noop,
    /// The result of an explicit cast: no conversion and no cast node in Elasticsearch's
    /// tree, only the explicit cast's own location, where a later compile-time cast of the
    /// result reports its error. Runtime locations stay those of the operand.
    Transparent,
}

#[derive(Debug)]
pub struct TExpr {
    pub loc: u32,
    pub ty: Ty,
    pub k: TK,
}

/// How a `def` binary/unary operator converts its result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DefOp {
    /// Static result type (the target type when one was known, `def` otherwise).
    pub ret: Ty,
    /// Explicit cast context: narrowing conversions allowed.
    pub explicit: bool,
    /// Compound assignment: cast back to the left operand's runtime class.
    pub compound: bool,
    /// The operands' parameter types in the call site's `MethodType` (they decide the guards
    /// `DefBootstrap` links, hence some error messages).
    pub args: [SiteArg; 2],
    /// The operands' static types (`def` for a unary operator's missing second operand).
    pub site: [Ty; 2],
}

/// An operand parameter of a `def` operator call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SiteArg {
    /// `def` / `Object`.
    #[default]
    Object,
    Primitive,
    /// Any other reference type (`String`, `Integer`, …).
    Reference,
}

impl SiteArg {
    pub fn of(t: Ty) -> SiteArg {
        if t == Ty::DEF || t == Ty::OBJECT {
            SiteArg::Object
        } else if t.is_primitive() {
            SiteArg::Primitive
        } else {
            SiteArg::Reference
        }
    }
}

#[derive(Debug)]
pub enum TK {
    Const(Const),
    Null,
    Cast(Box<TExpr>, Cast),
    /// `-x`, `~x`, `!x`, `+x` on a primitive type (`self.ty`), or `def` when `def` is set.
    Unary { op: Op, x: Box<TExpr>, def: Option<DefOp> },
    /// Arithmetic, bitwise and shifts on `operand` type (both sides already converted), or the
    /// dynamic version when `def` is set.
    Binary { op: Op, l: Box<TExpr>, r: Box<TExpr>, operand: Ty, def: Option<DefOp> },
    /// `=~` / `==~`.
    Regex { find: bool, l: Box<TExpr>, r: Box<TExpr> },
    Concat(Vec<TExpr>),
    /// Comparison with the comparison type (`def` compares dynamically).
    Comp { op: Op, l: Box<TExpr>, r: Box<TExpr>, operand: Ty },
    And(Box<TExpr>, Box<TExpr>),
    Or(Box<TExpr>, Box<TExpr>),
    /// `x instanceof T`; primitive operands are decided at compile time.
    InstanceOf { x: Box<TExpr>, ty: Ty, fixed: Option<bool> },
    Cond { c: Box<TExpr>, t: Box<TExpr>, f: Box<TExpr> },
    Elvis { l: Box<TExpr>, r: Box<TExpr> },
    NewList(Vec<TExpr>),
    NewMap(Vec<(TExpr, TExpr)>),
    /// `new T[a][b]`: `aty` is the array type.
    NewArray { dims: Vec<TExpr>, aty: Ty },
    /// `new T[] {…}`.
    ArrayInit(Vec<TExpr>, Ty),
    New { ctor: MethodId, args: Vec<TExpr> },
    /// A library method: static when `recv` is `None`.
    Invoke { recv: Option<Box<TExpr>>, m: MethodId, args: Vec<TExpr>, null_safe: bool },
    /// Method call on a `def` receiver.
    InvokeDyn { recv: Box<TExpr>, name: Box<str>, args: Vec<TExpr>, null_safe: bool, result: Option<Cast> },
    /// Call of a function declared in the script (or a synthetic lambda body).
    CallFunc { f: u32, args: Vec<TExpr> },
    /// A context method (`emit`, `field`, decay functions, …): `binding` indexes per-instance
    /// state for class bindings.
    CallContext { m: MethodId, args: Vec<TExpr>, binding: Option<u32> },
    Load(Box<Place>),
    /// `ploc` is the location of the assigned place (stores report errors there).
    Assign { place: Box<Place>, rhs: Box<TExpr>, compound: Option<Box<Compound>>, post: bool, read: bool, ploc: u32 },
    /// A lambda: the synthetic function and the captured locals.
    Lambda { f: u32, captures: Vec<u32>, iface: Option<ClassId> },
    /// A method reference (`String::length`, `this::f`, `x::foo`, `ArrayList::new`, `int[]::new`).
    FuncRef(Box<FuncRefSpec>),
    /// A compiled regex constant.
    Pattern(u32),
}

#[derive(Debug)]
pub enum FuncRefSpec {
    /// A static or unbound instance method on a class (`Integer::parseInt`, `String::length`).
    Class { class: ClassId, name: Box<str>, iface: Option<ClassId>, sym: Box<str> },
    /// A constructor (`ArrayList::new`).
    Ctor { class: ClassId, iface: Option<ClassId> },
    /// `T[]::new`.
    NewArray { ty: Ty, iface: Option<ClassId> },
    /// A script function (`this::f`).
    Local { name: Box<str>, iface: Option<ClassId> },
    /// A bound method on a captured variable (`x::foo`).
    Capture { slot: u32, name: Box<str>, iface: Option<ClassId> },
}

/// Something that can be loaded and (usually) stored.
#[derive(Debug)]
pub enum Place {
    Local { slot: u32 },
    /// `def` receiver field: map key, list index, getter/setter or field, decided at runtime.
    DefDot { recv: TExpr, name: Box<str>, null_safe: bool, result: Option<Cast> },
    /// A static field.
    StaticField { f: u32 },
    /// An instance field.
    Field { recv: TExpr, f: u32 },
    /// `x.foo` through `getFoo()` / `setFoo(v)`.
    Shortcut { recv: TExpr, getter: Option<MethodId>, setter: Option<MethodId> },
    /// `map.key` / `map[key]` through `get`/`put`.
    MapKey { recv: TExpr, key: TExpr, getter: Option<MethodId>, setter: Option<MethodId> },
    /// `list.0` / `list[i]` through `get`/`set`; `flip` wraps negative indexes (brace access).
    ListIdx { recv: TExpr, idx: TExpr, getter: Option<MethodId>, setter: Option<MethodId>, flip: bool },
    /// Array element (negative indexes count from the end).
    ArrayIdx { recv: TExpr, idx: TExpr },
    /// `.length` of an array.
    ArrayLen { recv: TExpr },
    /// `def[i]`: array, list or map at runtime.
    DefIdx { recv: TExpr, idx: TExpr, result: Option<Cast> },
}

/// Compound assignment details (`+=`, `++`, …).
#[derive(Debug)]
pub struct Compound {
    pub op: Op,
    /// The type the operation runs in.
    pub ty: Ty,
    /// Left value converted to `ty` before the operation.
    pub up: Option<Cast>,
    /// Result converted back to the left type.
    pub down: Option<Cast>,
    pub concat: bool,
}

/// A statement of the typed tree. Statement locations are kept for diagnostics even where
/// code generation does not need them.
#[derive(Debug)]
#[allow(dead_code)]
pub enum TStmt {
    Expr(TExpr),
    /// Declares (and initializes) a local; the initializer is already cast.
    Decl { slot: u32, ty: Ty, init: Option<TExpr>, loc: u32 },
    If { c: TExpr, then: Vec<TStmt>, els: Option<Vec<TStmt>> },
    While { c: Option<TExpr>, body: Vec<TStmt>, loc: u32 },
    DoWhile { c: Option<TExpr>, body: Vec<TStmt>, loc: u32 },
    For { init: Vec<TStmt>, c: Option<TExpr>, after: Option<TExpr>, body: Vec<TStmt>, loc: u32 },
    /// `for (x : array)`; `cast` converts each element to the variable type.
    EachArray { slot: u32, iterable: TExpr, cast: Option<Cast>, body: Vec<TStmt>, loc: u32 },
    /// `for (x : iterable)` (`def` iterables resolve at runtime).
    EachIter { slot: u32, iterable: TExpr, cast: Option<Cast>, body: Vec<TStmt>, loc: u32 },
    Return(Option<TExpr>, u32),
    Throw(TExpr, u32),
    Break,
    Continue,
    Try { body: Vec<TStmt>, catches: Vec<TCatch> },
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct TCatch {
    pub class: ClassId,
    pub slot: u32,
    pub body: Vec<TStmt>,
    pub loc: u32,
}

/// A function of the typed tree: `execute`, a script function, or a lambda body.
#[derive(Debug)]
pub struct TFunction {
    pub name: String,
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub body: Vec<TStmt>,
    /// Number of local slots (parameters first).
    pub nlocals: u32,
    /// Declared type of each local slot.
    pub local_tys: Vec<Ty>,
    pub loc: u32,
    /// Whether loops in this function count against the loop limit.
    pub loop_limit: bool,
}
