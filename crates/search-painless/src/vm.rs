//! The interpreter: a register machine over [`Value`]s.
//!
//! Each function owns a window of registers on one shared stack (locals first, then
//! temporaries); calls push a new window above the caller's. Instructions are typed by the
//! compiler (`AddI` adds two ints, `AddD` two doubles, `DefBin` dispatches dynamically), so the
//! common paths are a tag check and an arithmetic op. Library methods are plain Rust functions
//! called with their arguments moved out of the argument registers.
//!
//! Errors are Java exceptions ([`Throw`]) carrying the offset of the instruction that raised them
//! — the compiler records one source offset per instruction following Elasticsearch's line
//! number table — and are matched against each function's handler table for `try`/`catch`.

use std::any::Any;
use std::rc::Rc;
use std::sync::Arc;

use crate::host::Host;
use crate::ir::{Cast, DefOp};
use crate::lookup::{registry, MethodId, Registry};
use crate::types::{cls, ClassId, Ty};
use crate::value::{JException, JList, JMap, Obj, Value};

pub type Reg = u16;

/// A Java exception in flight.
#[derive(Debug)]
pub struct Throw {
    pub class: ClassId,
    pub message: Option<String>,
    /// Java frames of library code above the script frame (`java.base/...`), innermost first.
    pub frames: Vec<String>,
    /// Source offset of the script instruction that threw (set while unwinding).
    pub offset: Option<u32>,
    /// `Error`s (loop limit, `Debug.explain`, stack overflow) cannot be caught by scripts.
    pub catchable: bool,
    /// Elasticsearch exception type when it differs from the class name (`painless_error`,
    /// `circuit_breaking_exception`, …).
    pub es_type: Option<&'static str>,
    /// Metadata headers (`Debug.explain`).
    pub metadata: Vec<(String, String)>,
    /// The exception object, when a script threw one it constructed (rethrown as is).
    pub object: Option<Box<dyn Any>>,
    /// HTTP status when it is not 400 (circuit breakers answer 429).
    pub status: Option<u16>,
    /// The cause is an Elasticsearch exception, so it is reported as the root cause.
    pub es_root: bool,
    /// Extra fields of the cause in the error body.
    pub cause_fields: Vec<(String, serde_json::Value)>,
    /// `ParseException.getErrorOffset()` of a thrown `ParseException`.
    pub error_offset: Option<i32>,
}

pub type Thrown = Box<Throw>;

/// [`Throw::offset`] of an error whose Java stack trace was cut (at 1,024 frames) before the
/// script's frame: Elasticsearch then reports neither a position nor a script portion.
pub const NO_SCRIPT_FRAME: u32 = u32::MAX - 1;
pub type VR = Result<Value, Thrown>;

impl Throw {
    pub fn new(class: ClassId, message: impl Into<Option<String>>) -> Thrown {
        Box::new(Throw {
            class,
            message: message.into(),
            frames: Vec::new(),
            offset: None,
            catchable: true,
            es_type: None,
            metadata: Vec::new(),
            object: None,
            status: None,
            es_root: false,
            cause_fields: Vec::new(),
            error_offset: None,
        })
    }
    pub fn with_frames(mut self: Thrown, frames: &[&str]) -> Thrown {
        self.frames = frames.iter().map(|s| s.to_string()).collect();
        self
    }
    /// Records `cause` as this exception's `caused_by` (Java's `initCause`).
    pub fn with_cause(mut self: Thrown, cause: &Throw) -> Thrown {
        self.cause_fields.retain(|(k, _)| k != "caused_by");
        self.cause_fields.push(("caused_by".to_string(), cause.cause_json()));
        self
    }
    /// The `{type, reason, caused_by…}` body Elasticsearch renders for a nested cause.
    pub fn cause_json(&self) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        o.insert("type".into(), self.es_type().into());
        o.insert("reason".into(), self.message.clone().map(Into::into).unwrap_or(serde_json::Value::Null));
        for (k, v) in &self.cause_fields {
            o.insert(k.clone(), v.clone());
        }
        serde_json::Value::Object(o)
    }
    /// The Elasticsearch error type (`null_pointer_exception`, …).
    pub fn es_type(&self) -> String {
        if let Some(t) = self.es_type {
            return t.to_string();
        }
        let name = crate::lookup::class_name(self.class);
        let simple = name.rsplit('.').next().unwrap_or(name);
        crate::error::exception_name(simple)
    }
}

pub fn npe(msg: impl Into<String>) -> Thrown {
    Throw::new(cls::NULL_POINTER_EXCEPTION, Some(msg.into()))
}
pub fn cce(msg: impl Into<String>) -> Thrown {
    Throw::new(cls::CLASS_CAST_EXCEPTION, Some(msg.into()))
}
pub fn iae(msg: impl Into<String>) -> Thrown {
    Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(msg.into()))
}
pub fn ise(msg: impl Into<String>) -> Thrown {
    Throw::new(cls::ILLEGAL_STATE_EXCEPTION, Some(msg.into()))
}
pub fn uoe() -> Thrown {
    Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, None)
}
pub fn painless_error(msg: impl Into<String>) -> Thrown {
    let mut t = Throw::new(cls::THROWABLE, Some(msg.into()));
    t.catchable = false;
    t.es_type = Some("painless_error");
    t
}

/// One instruction. Registers are relative to the current frame.
#[derive(Clone, Debug)]
pub enum Insn {
    Mov { d: Reg, s: Reg },
    Const { d: Reg, k: u32 },
    Int { d: Reg, v: i32 },
    Null { d: Reg },
    Bool { d: Reg, v: bool },
    Cast { d: Reg, s: Reg, c: u32 },
    /// Cast `c` specialized for its common inputs: `int`/`long`/`float`/`double` widened to
    /// `double` (`ToD`), `int`/`long` to `long` (`ToL`), `int` to `int` (`ToI`); any other input
    /// runs the generic cast.
    ToD { d: Reg, s: Reg, c: u32 },
    ToL { d: Reg, s: Reg, c: u32 },
    ToI { d: Reg, s: Reg, c: u32 },

    AddI { d: Reg, a: Reg, b: Reg },
    SubI { d: Reg, a: Reg, b: Reg },
    MulI { d: Reg, a: Reg, b: Reg },
    DivI { d: Reg, a: Reg, b: Reg },
    RemI { d: Reg, a: Reg, b: Reg },
    AddL { d: Reg, a: Reg, b: Reg },
    SubL { d: Reg, a: Reg, b: Reg },
    MulL { d: Reg, a: Reg, b: Reg },
    DivL { d: Reg, a: Reg, b: Reg },
    RemL { d: Reg, a: Reg, b: Reg },
    AddF { d: Reg, a: Reg, b: Reg },
    SubF { d: Reg, a: Reg, b: Reg },
    MulF { d: Reg, a: Reg, b: Reg },
    DivF { d: Reg, a: Reg, b: Reg },
    RemF { d: Reg, a: Reg, b: Reg },
    AddD { d: Reg, a: Reg, b: Reg },
    SubD { d: Reg, a: Reg, b: Reg },
    MulD { d: Reg, a: Reg, b: Reg },
    DivD { d: Reg, a: Reg, b: Reg },
    RemD { d: Reg, a: Reg, b: Reg },
    /// Bitwise and shift operations on ints/longs/booleans.
    Bits { op: BitOp, d: Reg, a: Reg, b: Reg },
    NegI { d: Reg, s: Reg },
    NegL { d: Reg, s: Reg },
    NegF { d: Reg, s: Reg },
    NegD { d: Reg, s: Reg },
    NotI { d: Reg, s: Reg },
    NotL { d: Reg, s: Reg },
    Not { d: Reg, s: Reg },
    DefBin { op: crate::ast::Op, d: Reg, a: Reg, b: Reg, info: u32 },
    DefUn { op: crate::ast::Op, d: Reg, s: Reg, info: u32 },
    /// String concatenation of `n` consecutive registers.
    Concat { d: Reg, a: Reg, n: u16 },
    /// Typed comparison producing a boolean.
    Cmp { op: CmpOp, d: Reg, a: Reg, b: Reg },
    /// Fused compare-and-branch: jump to `t` when the comparison is false.
    JCmpFalse { op: CmpOp, a: Reg, b: Reg, t: u32 },
    /// Fused compare-and-branch: jump to `t` when the comparison is true. With `lc`, taking the
    /// branch counts one loop iteration (the loop's back edge).
    JCmp { op: CmpOp, a: Reg, b: Reg, t: u32, lc: bool },
    /// Int comparison with an immediate: jump to `t` when the result equals `when`.
    JCmpK { op: CmpOp, a: Reg, k: i32, t: u32, when: bool, lc: bool },
    /// Charges a fixed allocation against `max_allocation_bytes` (a boxed primitive, an
    /// array initializer, a lambda's captures).
    Alloc { bytes: u32 },
    /// Charges `new T[a]...[a + n - 1]`: the innermost elements, `w` bytes each.
    AllocArray { a: Reg, n: u8, w: u8 },
    /// Charges the string concatenation of registers `a..a + n` (`Program::concat_infos`).
    AllocConcat { a: Reg, n: u16, info: u32 },
    /// A counting loop's back edge: `a += inc`, then `JCmpK` on the new value. The loop's
    /// entry still reaches the plain `JCmpK` that follows.
    IncJCmpK { op: CmpOp, a: Reg, inc: i32, k: i32, t: u32, when: bool, lc: bool },
    /// Int addition of an immediate (`++i`, `x += 1`, `n - 1`).
    AddIK { d: Reg, a: Reg, k: i32 },
    DefCmp { op: crate::ast::Op, d: Reg, a: Reg, b: Reg, info: u32 },

    Jmp { t: u32 },
    /// Jump when the boolean register is false.
    Jf { c: Reg, t: u32 },
    /// Jump when the boolean register is true.
    Jt { c: Reg, t: u32 },
    JNull { s: Reg, t: u32 },
    JNonNull { s: Reg, t: u32 },
    Ret { s: Reg },
    RetVoid,
    Throw { s: Reg },
    LoopCheck,

    /// Library method or constructor; arguments (receiver first) in `a..a+n`.
    Call { d: Reg, m: MethodId, a: Reg, n: u8 },
    /// Instance method: like `Call` but throws a `NullPointerException` on a null receiver.
    CallRecv { d: Reg, m: MethodId, a: Reg, n: u8 },
    CallDyn { d: Reg, name: u32, a: Reg, n: u8, site: u32, res: u32 },
    CallFunc { d: Reg, f: u32, a: Reg, n: u8 },
    CallCtx { d: Reg, m: MethodId, a: Reg, n: u8, binding: u32, ctor_loc: u32 },
    StaticField { d: Reg, f: u32 },
    Field { d: Reg, r: Reg, f: u32 },
    GetDot { d: Reg, r: Reg, name: u32, res: u32 },
    SetDot { r: Reg, name: u32, v: Reg },
    /// Array element access; `ty` (a type-pool index) is the static array type, which only the
    /// null-array message needs.
    ArrLoad { d: Reg, r: Reg, i: Reg, ty: u32 },
    ArrStore { r: Reg, i: Reg, v: Reg, ty: u32 },
    ArrLen { d: Reg, r: Reg },
    /// Wraps a negative index (`a[-1]`, `list[-1]`) around the collection's length.
    Flip { d: Reg, r: Reg, i: Reg, list: bool },
    /// `Def.normalizeIndex` for dynamic brace access (fails before the access itself).
    /// `t`: the index's static type when `MethodHandle.asType` cannot convert it to `int`
    /// (array and list receivers then fail to link), else `u32::MAX`.
    DefNorm { d: Reg, r: Reg, i: Reg, t: u32 },
    /// Dynamic brace load/store with a normalized index; `vty` is the static value type of a
    /// store when primitive (`u32::MAX` otherwise).
    GetIdx { d: Reg, r: Reg, i: Reg, res: u32 },
    SetIdx { r: Reg, i: Reg, v: Reg, vty: u32 },
    NewList { d: Reg, a: Reg, n: u16 },
    NewMap { d: Reg, a: Reg, n: u16 },
    NewArray { d: Reg, ty: u32, a: Reg, n: u8 },
    ArrayInit { d: Reg, ty: u32, a: Reg, n: u16 },
    InstanceOf { d: Reg, s: Reg, ty: u32 },
    Lambda { d: Reg, f: u32, a: Reg, n: u8, iface: u32 },
    FuncRef { d: Reg, spec: u32, cap: Reg },
    Pattern { d: Reg, p: u32 },
    RegexOp { find: bool, d: Reg, s: Reg, p: Reg },
    IterInit { d: Reg, s: Reg },
    /// Advances iterator `it` into `d`, jumping to `t` when exhausted.
    /// `p`: 0, or 1 + the `Prim` a for-each over `def` converts elements to (`ValueIterator`).
    IterNext { d: Reg, it: Reg, t: u32, p: u8 },
    /// Reads `doc[field].value` (or `.size()`/`.empty`) without materializing doc values.
    DocValue { d: Reg, field: u32, what: DocWhat },
    /// Value `i` of `doc[field]` (fetched already) as a for-each loop over it sees it: boxed
    /// (`p` = 0) or converted to a primitive variable (1 + its `Prim`, see `IterNext`).
    DocAt { d: Reg, field: u32, i: Reg, p: u8 },
    /// Loads a context variable (`params`, `ctx`, `_score`, …) into a register.
    LoadVar { d: Reg, var: u32 },
    /// `Map.get` with a constant string key (`params.x`, `m['k']`): method `m` called on `r`,
    /// with a per-site cache of where the key sits in the map.
    MapGetK { d: Reg, r: Reg, k: u32, m: MethodId, site: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocWhat {
    /// Loads the field's doc values for the current document (no result).
    Fetch,
    Value,
    Size,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitOp {
    AndI,
    OrI,
    XorI,
    AndL,
    OrL,
    XorL,
    AndZ,
    OrZ,
    XorZ,
    ShlI,
    ShrI,
    UshrI,
    ShlL,
    ShrL,
    UshrL,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    EqI,
    NeI,
    LtI,
    LeI,
    GtI,
    GeI,
    EqL,
    NeL,
    LtL,
    LeL,
    GtL,
    GeL,
    EqF,
    NeF,
    LtF,
    LeF,
    GtF,
    GeF,
    EqD,
    NeD,
    LtD,
    LeD,
    GtD,
    GeD,
    EqZ,
    NeZ,
    /// `Object.equals` semantics (null-safe).
    EqObj,
    NeObj,
    /// Reference identity (`===`).
    EqRef,
    NeRef,
}

#[derive(Clone, Debug)]
pub struct Handler {
    pub start: u32,
    pub end: u32,
    pub target: u32,
    pub class: ClassId,
    pub reg: Reg,
}

/// A compiled function.
#[derive(Debug)]
pub struct Function {
    pub name: String,
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub nregs: u16,
    pub code: Vec<Insn>,
    /// Source offset (bytes) per instruction, for error positions.
    pub locs: Vec<u32>,
    pub handlers: Vec<Handler>,
    pub loop_limit: bool,
    /// Registers that receive captured values when the function is a lambda.
    pub captures: Vec<Reg>,
    pub loc: u32,
    /// Helpful-NPE receiver descriptions by instruction index.
    pub null_descs: Vec<(u32, Box<str>)>,
}

impl Function {
    pub fn null_desc(&self, pc: usize) -> Option<&str> {
        self.null_descs.iter().find(|(p, _)| *p as usize == pc).map(|(_, d)| &**d)
    }
}

/// Constant objects of a program, materialized into values per instance: regex literals and
/// the extractors `grok`/`dissect` build at compile time.
#[derive(Debug)]
pub enum ProgConst {
    Regex(Arc<crate::regex::Regex>),
    Extractor(Arc<crate::es::extract::Extractor>),
}

/// Compile-time constants, materialized into values per instance.
#[derive(Clone, Debug)]
pub enum ConstVal {
    Long(i64),
    Float(f32),
    Double(f64),
    Str(Arc<str>),
    Byte(i8),
    Short(i16),
    Char(u16),
}

/// A compiled script. Immutable and shareable across threads.
#[derive(Debug)]
pub struct Program {
    pub source: Arc<str>,
    pub functions: Vec<Function>,
    pub consts: Vec<ConstVal>,
    pub casts: Vec<Cast>,
    pub types: Vec<Ty>,
    pub names: Vec<Box<str>>,
    pub defops: Vec<DefOp>,
    pub funcrefs: Vec<crate::func::RefSpec>,
    pub patterns: Vec<ProgConst>,
    pub doc_fields: Vec<Box<str>>,
    pub execute: u32,
    /// Statement starts (byte offsets), sorted.
    pub statements: Vec<u32>,
    pub used_vars: Vec<bool>,
    pub bindings: u32,
    pub dyn_sites: u32,
    /// Static argument types and result type of each `def` call site.
    pub dyn_sigs: Vec<(Vec<Ty>, Ty)>,
    /// What each `def` operator call site has linked so far ([`crate::defmath::link`]),
    /// shared by every execution of the script like Painless's `MutableCallSite`s.
    pub def_links: Vec<std::sync::atomic::AtomicU64>,
    /// The expression fast path ([`crate::fast`]) has run the script: its `def` operator
    /// sites count as linked (to classes it did not record).
    pub fast_ran: std::sync::atomic::AtomicBool,
    pub map_sites: u32,
    pub max_loop_counter: u32,
    pub regex_limit_factor: i32,
    pub uses_score: bool,
    /// `script.painless.max_allocation_bytes` (0: allocations are not charged).
    pub max_allocation_bytes: u64,
    /// See `Insn::AllocConcat`.
    pub concat_infos: Vec<(u64, Vec<bool>)>,
}

/// Per-instance mutable state (one per thread/segment): constants as values, caches, state of
/// class bindings and runtime-field emits.
pub struct State {
    pub consts: Vec<Value>,
    pub patterns: Vec<Value>,
    /// Context variables by index in the context's variable list.
    pub vars: Vec<Value>,
    /// Per-call-site state of class bindings (`randomScore`, decay functions…).
    pub bindings: Vec<Option<Box<dyn Any>>>,
    /// Monomorphic cache of `def` call sites: receiver class and resolved method.
    pub dyn_cache: Vec<(ClassId, MethodId)>,
    /// Values emitted by runtime-field scripts during the current execution.
    pub emitted: Vec<Value>,
    /// Reused doc-values buffers, one per literal `doc['field']` of the program.
    pub doc_bufs: Vec<crate::host::DocValues>,
    /// Doc-values buffers for dynamic `doc[expr]` lookups, by field name.
    pub doc_dyn: Vec<(Box<str>, crate::host::DocValues)>,
    /// The `doc[name]` value of each `doc_dyn` field and the document generation it holds:
    /// `LeafDocLookup` hands out one `ScriptDocValues` per field, refilled per document.
    pub doc_sdv: Vec<(u64, Value)>,
    /// Bumped per document so cached per-document values (`doc` maps) are refreshed.
    pub doc_gen: u64,
    pub cur_binding: u32,
    pub depth: u32,
    /// Document generation each literal doc-values buffer was filled for.
    pub doc_buf_gen: Vec<u64>,
    /// Whether the context has a `doc` variable (`params.doc` then resolves to it).
    pub has_doc: bool,
    /// Index of the `ctx` variable of write contexts (`metadata()`, `field()`).
    pub ctx_var: Option<u32>,
    /// Runtime field name (`emit` limit messages, composite sub-field prefixes).
    pub field_name: std::rc::Rc<str>,
    /// Characters emitted by a keyword runtime field during the current execution.
    pub emit_chars: u64,
    /// Values emitted per sub-field by a composite runtime field, in first-emit order.
    pub emitted_fields: Vec<(String, Vec<Value>)>,
    /// Source offset a class binding's construction failures are reported at.
    pub binding_loc: u32,
    /// `explanation.set(…)` of a score script during the current execution.
    pub explanation: Option<String>,
    /// Suspended callers of the interpreter loop.
    pub frames: Vec<Frame>,
    /// `MapGetK` caches: the map last seen at the site and the entry index of the key in it.
    pub map_cache: Vec<(usize, u32)>,
    /// Overrides found for Elasticsearch objects behind interface-typed calls
    /// ([`crate::es::virtual_imp`]): (method, receiver class, implementation).
    pub virt: Vec<(MethodId, ClassId, crate::lookup::Native)>,
    /// Reused buffer for the document vectors of vector score functions.
    pub vec_buf: crate::host::DocValues,
    /// Locals of the expression fast path, as raw bits ([`crate::fast`]).
    pub fast_locals: Vec<u64>,
    /// Bytes charged against `max_allocation_bytes` in this execution (Java `long`).
    pub alloc_total: i64,
    /// Loops poll [`crate::Host::check_cancelled`] instead of counting statements (see
    /// [`crate::Instance::set_cancellable`]).
    pub cancellable: bool,
}

impl State {
    pub fn new(prog: &Program, consts: Vec<Value>, patterns: Vec<Value>, vars: Vec<Value>) -> State {
        State {
            consts,
            patterns,
            vars,
            bindings: (0..prog.bindings).map(|_| None).collect(),
            dyn_cache: vec![(ClassId(u16::MAX), u32::MAX); prog.dyn_sites as usize],
            emitted: Vec::new(),
            doc_bufs: (0..prog.doc_fields.len()).map(|_| crate::host::DocValues::default()).collect(),
            doc_dyn: Vec::new(),
            doc_sdv: Vec::new(),
            doc_gen: 0,
            cur_binding: u32::MAX,
            depth: 0,
            doc_buf_gen: vec![0; prog.doc_fields.len()],
            has_doc: false,
            ctx_var: None,
            field_name: std::rc::Rc::from(""),
            emit_chars: 0,
            emitted_fields: Vec::new(),
            explanation: None,
            binding_loc: 0,
            frames: Vec::new(),
            map_cache: vec![(0, 0); prog.map_sites as usize],
            virt: Vec::new(),
            vec_buf: crate::host::DocValues::default(),
            fast_locals: Vec::new(),
            alloc_total: 0,
            cancellable: false,
        }
    }

    /// Resets per-document state before an execution.
    pub fn begin_document(&mut self) {
        self.emitted.clear();
        self.emitted_fields.clear();
        self.emit_chars = 0;
        self.explanation = None;
        self.doc_gen = self.doc_gen.wrapping_add(1);
        self.depth = 0;
        self.frames.clear();
        self.alloc_total = 0;
    }
}

/// Script call depth (Elasticsearch overflows the JVM stack somewhere between 4,000 and 8,000
/// frames, depending on frame sizes).
const MAX_DEPTH: u32 = 6000;
/// Registers all frames of one execution may hold.
const MAX_STACK: usize = 1 << 20;

/// A suspended caller (or the running function) of the interpreter loop.
#[derive(Clone, Copy)]
pub struct Frame {
    f: u32,
    base: usize,
    pc: usize,
    counter: u32,
    /// Caller register receiving the result.
    ret: Reg,
}

/// How [`Vm::run`] leaves a function's code.
enum Exit {
    /// Return the value of a register.
    Ret(Reg),
    RetVoid,
    Call { f: u32, d: Reg, a: Reg, n: u8 },
}

use crate::guard::stack_overflow;

/// The interpreter for one run.
pub struct Vm<'a> {
    pub prog: &'a Program,
    pub st: &'a mut State,
    pub host: &'a mut dyn Host,
    pub stack: &'a mut Vec<Value>,
    pub reg: &'static Registry,
}

impl<'a> Vm<'a> {
    pub fn new(prog: &'a Program, st: &'a mut State, host: &'a mut dyn Host, stack: &'a mut Vec<Value>) -> Vm<'a> {
        Vm { prog, st, host, stack, reg: registry() }
    }

    /// Calls a script function with arguments (already converted to its parameter types) and
    /// captured values.
    pub fn call_function(&mut self, f: u32, args: &[Value], captures: &[Value]) -> VR {
        let prog: &'a Program = self.prog;
        let func = &prog.functions[f as usize];
        // guard the native stack too: calls from library code (comparators, `forEach`) recurse
        // through Rust frames
        if self.st.depth >= MAX_DEPTH || crate::guard::stack_exhausted() {
            return Err(stack_overflow());
        }
        let base = self.stack.len();
        push_frame(self.stack, func.nregs as usize);
        for (i, a) in args.iter().enumerate() {
            self.stack[base + i] = a.clone();
        }
        for (i, c) in captures.iter().enumerate() {
            if let Some(&r) = func.captures.get(i) {
                self.stack[base + r as usize] = c.clone();
            }
        }
        self.st.depth += 1;
        let r = match self.exec(f, base) {
            Ok(Some(s)) => Ok(take_reg(&mut self.stack[base + s as usize])),
            Ok(None) => Ok(Value::Null),
            Err(t) => Err(t),
        };
        self.st.depth -= 1;
        pop_frame(self.stack, base);
        r
    }

    /// Runs the script's entry function (no arguments) and hands its result to `read` while it
    /// is still in its register. Hot loops (`script_score`) read a double straight from the
    /// register this way: moving the value out through memory first costs a store-forwarding
    /// stall per document.
    #[inline]
    pub fn call_entry<R>(&mut self, f: u32, read: impl FnOnce(&mut Value) -> R) -> Result<R, Thrown> {
        let base = self.stack.len();
        push_frame(self.stack, self.prog.functions[f as usize].nregs as usize);
        self.st.depth += 1;
        let r = match self.exec(f, base) {
            Ok(Some(s)) => Ok(read(&mut self.stack[base + s as usize])),
            Ok(None) => Ok(read(&mut Value::Null)),
            Err(t) => Err(t),
        };
        self.st.depth -= 1;
        pop_frame(self.stack, base);
        r
    }

    #[inline(always)]
    fn r(&self, base: usize, r: Reg) -> &Value {
        debug_assert!(base + (r as usize) < self.stack.len());
        // SAFETY: `call_function` sizes the frame to `nregs` registers and the peephole pass
        // asserts every register operand of the function is below `nregs`.
        unsafe { self.stack.get_unchecked(base + r as usize) }
    }

    /// Stores the result of an out-of-line call (see [`move_value`]), or passes its error on.
    #[inline(always)]
    fn set_vr(&mut self, base: usize, r: Reg, res: VR) -> Result<(), Thrown> {
        match &res {
            Ok(v) => {
                debug_assert!(base + (r as usize) < self.stack.len());
                // SAFETY: as in `r`; ownership moves to the register and `res` is forgotten
                unsafe {
                    let slot = self.stack.as_mut_ptr().add(base + r as usize);
                    if (*slot).is_heap() {
                        drop_heap(slot);
                    }
                    move_value(slot, v);
                }
                std::mem::forget(res);
                Ok(())
            }
            Err(_) => Err(res.unwrap_err()),
        }
    }

    #[inline(always)]
    fn set(&mut self, base: usize, r: Reg, v: Value) {
        debug_assert!(base + (r as usize) < self.stack.len());
        // SAFETY: as in `r`; the old value is dropped before the slot is overwritten
        unsafe {
            let slot = self.stack.as_mut_ptr().add(base + r as usize);
            if (*slot).is_heap() {
                drop_heap(slot);
            }
            write_value(slot, v);
        }
    }

    /// Runs function `f` whose frame starts at `base`. Calls between script functions do not
    /// recurse natively: the caller's position is saved in `State::frames` and the callee runs
    /// in the same loop, so recursion depth is bounded by `MAX_DEPTH`, not the native stack.
    /// Returns the register holding the result (`None` for `void`), still in the frame at
    /// `base`, which the caller pops.
    fn exec(&mut self, f: u32, base: usize) -> Result<Option<Reg>, Thrown> {
        let prog: &'a Program = self.prog;
        let reg: &'static Registry = self.reg;
        let floor = self.st.frames.len();
        let mut cur = Frame { f, base, pc: 0, counter: self.loop_budget(&prog.functions[f as usize]), ret: 0 };
        loop {
            let func = &prog.functions[cur.f as usize];
            let err = match self.run(func, cur.base, &mut cur.pc, &mut cur.counter) {
                Ok(exit @ (Exit::Ret(_) | Exit::RetVoid)) => {
                    if self.st.frames.len() == floor {
                        return Ok(match exit {
                            Exit::Ret(s) => Some(s),
                            _ => None,
                        });
                    }
                    let v = match exit {
                        Exit::Ret(s) => take_reg(&mut self.stack[cur.base + s as usize]),
                        _ => Value::Null,
                    };
                    pop_frame(self.stack, cur.base);
                    self.st.depth -= 1;
                    let d = cur.ret;
                    cur = self.st.frames.pop().unwrap();
                    self.set(cur.base, d, v);
                    continue;
                }
                Ok(Exit::Call { f: callee, d, a, n }) => {
                    let cf = &prog.functions[callee as usize];
                    if self.st.depth >= MAX_DEPTH || self.stack.len() + cf.nregs as usize > MAX_STACK {
                        stack_overflow()
                    } else {
                        let nb = self.stack.len();
                        push_frame(self.stack, cf.nregs as usize);
                        for i in 0..n as usize {
                            let v = std::mem::take(&mut self.stack[cur.base + a as usize + i]);
                            self.stack[nb + i] = v;
                        }
                        self.st.depth += 1;
                        self.st.frames.push(cur);
                        let counter = self.loop_budget(cf);
                        cur = Frame { f: callee, base: nb, pc: 0, counter, ret: d };
                        continue;
                    }
                }
                Err(t) => t,
            };
            // unwind: the innermost frame locates the error, each frame's handlers get a chance
            let mut t = err;
            loop {
                let func = &prog.functions[cur.f as usize];
                let at = cur.pc.saturating_sub(1);
                if t.offset.is_none() {
                    t.offset = func.locs.get(at).copied();
                }
                if t.catchable
                    && let Some(h) = func.handlers.iter().find(|h| (h.start as usize) <= at && at < h.end as usize && reg.is_assignable(Ty::of(h.class), Ty::of(t.class)))
                {
                    let (target, hreg) = (h.target as usize, h.reg);
                    let ex = exception_value(*t);
                    self.set(cur.base, hreg, ex);
                    cur.pc = target;
                    break;
                }
                if self.st.frames.len() == floor {
                    return Err(t);
                }
                pop_frame(self.stack, cur.base);
                self.st.depth -= 1;
                cur = self.st.frames.pop().unwrap();
            }
        }
    }

    /// The loop counter a new frame of `f` starts with: the statement limit, or in a
    /// cancellable execution the iterations until the next cancellation poll (0: no loops).
    #[inline(always)]
    fn loop_budget(&self, f: &Function) -> u32 {
        if !f.loop_limit {
            0
        } else if self.st.cancellable {
            CANCELLATION_POLL_INTERVAL
        } else {
            self.prog.max_loop_counter
        }
    }

    /// A frame's loop counter ran out. Elasticsearch 9.5 contexts that a search can cancel
    /// (score, filter, fields, sort, aggregations…) run their loops without a statement limit
    /// and poll for cancellation every 1,000 iterations instead; elsewhere the script has
    /// executed `max_loop_counter` loop statements and fails.
    #[cold]
    #[inline(never)]
    fn loop_exhausted(&mut self, counter: &mut u32) -> Result<(), Thrown> {
        if !self.st.cancellable {
            return Err(painless_error("The maximum number of statements that can be executed in a loop has been reached."));
        }
        *counter = CANCELLATION_POLL_INTERVAL;
        if let Some(t) = crate::guard::take_pending() {
            return Err(t);
        }
        self.host.check_cancelled().map_err(crate::es::doc::host_error)
    }

    /// `$checkAllocBytes`: the running total (a Java `long`) over the limit fails the script.
    #[cold]
    fn charge(&mut self, bytes: i64) -> Result<(), Thrown> {
        let limit = self.prog.max_allocation_bytes as i64;
        self.st.alloc_total = self.st.alloc_total.wrapping_add(bytes);
        if limit > 0 && self.st.alloc_total > limit {
            return Err(painless_error(format!(
                "script allocation limit exceeded: allocation of [{bytes}] bytes brings the running total to [{}] bytes, over the limit of [{limit}] bytes",
                self.st.alloc_total
            )));
        }
        Ok(())
    }

    /// [`crate::es::virtual_imp`] with the resolved overrides cached per instance.
    #[inline(always)]
    fn virtual_imp(&mut self, m: MethodId, args: &[Value]) -> crate::lookup::Native {
        if let Some(Value::Obj(o)) = args.first()
            && let Obj::Es(e) = &**o
        {
            let c = e.class();
            if let Some(&(_, _, imp)) = self.st.virt.iter().find(|(mm, cc, _)| *mm == m && *cc == c) {
                return imp;
            }
            let imp = crate::es::virtual_imp(self.reg, m, args);
            self.st.virt.push((m, c, imp));
            return imp;
        }
        self.reg.method(m).imp
    }

    /// Runs a library method. A `subList` view receiver (or first argument) is brought up to
    /// date with its parent before and written back after.
    #[inline(always)]
    pub fn call_native(&mut self, imp: crate::lookup::Native, m: crate::lookup::MethodId, args: &mut [Value]) -> VR {
        if let Some(Value::List(l)) = args.first()
            && l.view.is_some()
        {
            let l = l.clone();
            let mi = self.reg.method(m);
            crate::lib_coll::view_enter(&l, &crate::lib_coll::ViewCall { owner: mi.owner, name: &mi.name, arity: mi.params.len() })?;
            let r = imp(self, args);
            crate::lib_coll::view_exit(&l);
            return overflowed(r);
        }
        overflowed(imp(self, args))
    }

    #[inline(always)]
    fn take_args(&mut self, base: usize, a: Reg, n: u8) -> smallvec_args::Args {
        let start = base + a as usize;
        let end = start + n as usize;
        assert!(end <= self.stack.len());
        let mut args = smallvec_args::Args::new();
        for i in start..end {
            // SAFETY: in bounds (asserted above)
            unsafe { args.take_from(self.stack.as_mut_ptr().add(i)) };
        }
        args
    }

    /// The dispatch loop. Arithmetic, moves, constants, compares and jumps run here with the
    /// frame's registers behind a raw pointer, the program counter and the loop counter in
    /// locals; everything else goes through [`Vm::slow`], after which the register pointer is
    /// reloaded (library code may grow the register stack by calling back into lambdas).
    fn run(&mut self, func: &Function, base: usize, pc_ref: &mut usize, counter: &mut u32) -> Result<Exit, Thrown> {
        use crate::vm::Insn::*;
        let prog: &'a Program = self.prog;
        let code = func.code.as_ptr();
        // the instruction pointer rather than an index: one register less in this loop, where
        // register pressure otherwise spills the program counter to memory
        // SAFETY: `pc_ref` is a valid instruction index (or one past a jump target)
        let mut ip: *const Insn = unsafe { code.add(*pc_ref) };
        macro_rules! pc_now {
            () => {
                // SAFETY: `ip` points into `func.code`
                unsafe { ip.offset_from(code) as usize }
            };
        }
        // the loop counter stays in `*counter`: it only changes on back edges, and keeping it
        // out of a register leaves one for the instruction pointer
        // SAFETY: the frame holds `func.nregs` registers from `base` (see `r`).
        let mut regs = unsafe { self.stack.as_mut_ptr().add(base) };
        macro_rules! r {
            ($x:expr) => {
                // SAFETY: register operands are below `nregs` (asserted by the peephole pass)
                unsafe { &*regs.add($x as usize) }
            };
        }
        // `$v` must not read registers: it is evaluated after the old value is dropped, so the
        // new value is stored straight into the register
        macro_rules! put {
            ($x:expr, $v:expr) => {{
                // SAFETY: as in `r!`
                let slot = unsafe { regs.add($x as usize) };
                unsafe {
                    if (*slot).is_heap() {
                        drop_heap(slot);
                    }
                    write_value(slot, $v);
                }
            }};
        }
        // typed results: tag and payload words (see `store_prim`)
        macro_rules! prim {
            ($x:expr, $tag:expr, $bits:expr) => {{
                let bits: u64 = $bits;
                // SAFETY: as in `r!`
                let slot = unsafe { regs.add($x as usize) };
                unsafe {
                    if (*slot).is_heap() {
                        drop_heap(slot);
                    }
                    store_prim(slot, $tag, bits);
                }
            }};
        }
        macro_rules! int_op {
            ($x:expr, $e:expr) => {{
                let x: i32 = $e;
                prim!($x, tag::INT, x as u32 as u64)
            }};
        }
        macro_rules! long_op {
            ($x:expr, $e:expr) => {{
                let x: i64 = $e;
                prim!($x, tag::LONG, x as u64)
            }};
        }
        macro_rules! float_op {
            ($x:expr, $e:expr) => {{
                let x: f32 = $e;
                prim!($x, tag::FLOAT, x.to_bits() as u64)
            }};
        }
        macro_rules! double_op {
            ($x:expr, $e:expr) => {{
                let x: f64 = $e;
                prim!($x, tag::DOUBLE, x.to_bits())
            }};
        }
        macro_rules! bool_op {
            ($x:expr, $e:expr) => {{
                let x: bool = $e;
                prim!($x, tag::BOOL, x as u64)
            }};
        }
        // the result of an out-of-line call: moved from where the callee wrote it, field by
        // field (see `move_value`)
        macro_rules! put_vr {
            ($x:expr, $e:expr) => {{
                let res: VR = $e;
                match &res {
                    Ok(v) => {
                        // SAFETY: as in `r!`; ownership moves to the register, `res` is forgotten
                        let slot = unsafe { regs.add($x as usize) };
                        unsafe {
                            if (*slot).is_heap() {
                                drop_heap(slot);
                            }
                            move_value(slot, v);
                        }
                        std::mem::forget(res);
                    }
                    Err(_) => throw!(res.unwrap_err()),
                }
            }};
        }
        macro_rules! slow {
            ($insn:expr) => {{
                *pc_ref = pc_now!();
                if let Some(exit) = self.slow(func, base, $insn, pc_ref, counter)? {
                    return Ok(exit);
                }
                // SAFETY: `slow` leaves a valid instruction index
                ip = unsafe { code.add(*pc_ref) };
                // SAFETY: as above; the stack may have been reallocated
                regs = unsafe { self.stack.as_mut_ptr().add(base) };
            }};
        }
        macro_rules! throw {
            ($e:expr) => {{
                *pc_ref = pc_now!();
                return Err($e);
            }};
        }
        loop {
            debug_assert!(pc_now!() < func.code.len());
            // SAFETY: jump targets are in range and every function ends in a return (asserted by
            // the peephole pass)
            let insn = unsafe { &*ip };
            ip = unsafe { ip.add(1) };
            match *insn {
                Mov { d, s } => {
                    let src = r!(s);
                    if src.is_heap() {
                        let v = src.clone();
                        put!(d, v);
                    } else if d != s {
                        // SAFETY: as in `r!`; `src` owns no heap data
                        let slot = unsafe { regs.add(d as usize) };
                        unsafe {
                            if (*slot).is_heap() {
                                drop_heap(slot);
                            }
                            copy_prim(slot, src);
                        }
                    }
                }
                Const { d, k } => {
                    let c = &self.st.consts[k as usize];
                    if c.is_heap() {
                        let v = c.clone();
                        put!(d, v);
                    } else {
                        // SAFETY: as in `r!`; constants are not heap values here
                        let slot = unsafe { regs.add(d as usize) };
                        unsafe {
                            if (*slot).is_heap() {
                                drop_heap(slot);
                            }
                            copy_prim(slot, c);
                        }
                    }
                }
                Int { d, v } => int_op!(d, v),
                Null { d } => prim!(d, tag::NULL, 0),
                Bool { d, v } => bool_op!(d, v),
                AddI { d, a, b } => int_op!(d, ii(r!(a)).wrapping_add(ii(r!(b)))),
                SubI { d, a, b } => int_op!(d, ii(r!(a)).wrapping_sub(ii(r!(b)))),
                MulI { d, a, b } => int_op!(d, ii(r!(a)).wrapping_mul(ii(r!(b)))),
                AddIK { d, a, k } => int_op!(d, ii(r!(a)).wrapping_add(k)),
                DivI { d, a, b } => {
                    let y = ii(r!(b));
                    if y == 0 {
                        throw!(div_zero());
                    }
                    int_op!(d, ii(r!(a)).wrapping_div(y))
                }
                RemI { d, a, b } => {
                    let y = ii(r!(b));
                    if y == 0 {
                        throw!(div_zero());
                    }
                    int_op!(d, ii(r!(a)).wrapping_rem(y))
                }
                AddL { d, a, b } => long_op!(d, ll(r!(a)).wrapping_add(ll(r!(b)))),
                SubL { d, a, b } => long_op!(d, ll(r!(a)).wrapping_sub(ll(r!(b)))),
                MulL { d, a, b } => long_op!(d, ll(r!(a)).wrapping_mul(ll(r!(b)))),
                DivL { d, a, b } => {
                    let y = ll(r!(b));
                    if y == 0 {
                        throw!(div_zero());
                    }
                    long_op!(d, ll(r!(a)).wrapping_div(y))
                }
                RemL { d, a, b } => {
                    let y = ll(r!(b));
                    if y == 0 {
                        throw!(div_zero());
                    }
                    long_op!(d, ll(r!(a)).wrapping_rem(y))
                }
                AddF { d, a, b } => float_op!(d, ff(r!(a)) + ff(r!(b))),
                SubF { d, a, b } => float_op!(d, ff(r!(a)) - ff(r!(b))),
                MulF { d, a, b } => float_op!(d, ff(r!(a)) * ff(r!(b))),
                DivF { d, a, b } => float_op!(d, ff(r!(a)) / ff(r!(b))),
                RemF { d, a, b } => float_op!(d, ff(r!(a)) % ff(r!(b))),
                AddD { d, a, b } => double_op!(d, dd(r!(a)) + dd(r!(b))),
                SubD { d, a, b } => double_op!(d, dd(r!(a)) - dd(r!(b))),
                MulD { d, a, b } => double_op!(d, dd(r!(a)) * dd(r!(b))),
                DivD { d, a, b } => double_op!(d, dd(r!(a)) / dd(r!(b))),
                RemD { d, a, b } => double_op!(d, dd(r!(a)) % dd(r!(b))),
                Bits { op, d, a, b } => {
                    let (x, y) = (r!(a), r!(b));
                    match op {
                        BitOp::AndI => int_op!(d, ii(x) & ii(y)),
                        BitOp::OrI => int_op!(d, ii(x) | ii(y)),
                        BitOp::XorI => int_op!(d, ii(x) ^ ii(y)),
                        BitOp::AndL => long_op!(d, ll(x) & ll(y)),
                        BitOp::OrL => long_op!(d, ll(x) | ll(y)),
                        BitOp::XorL => long_op!(d, ll(x) ^ ll(y)),
                        BitOp::AndZ => bool_op!(d, zz(x) & zz(y)),
                        BitOp::OrZ => bool_op!(d, zz(x) | zz(y)),
                        BitOp::XorZ => bool_op!(d, zz(x) ^ zz(y)),
                        BitOp::ShlI => int_op!(d, ii(x).wrapping_shl(ii(y) as u32)),
                        BitOp::ShrI => int_op!(d, ii(x).wrapping_shr(ii(y) as u32)),
                        BitOp::UshrI => int_op!(d, (ii(x) as u32).wrapping_shr(ii(y) as u32) as i32),
                        BitOp::ShlL => long_op!(d, ll(x).wrapping_shl(ii(y) as u32)),
                        BitOp::ShrL => long_op!(d, ll(x).wrapping_shr(ii(y) as u32)),
                        BitOp::UshrL => long_op!(d, (ll(x) as u64).wrapping_shr(ii(y) as u32) as i64),
                    }
                }
                NegI { d, s } => int_op!(d, ii(r!(s)).wrapping_neg()),
                NegL { d, s } => long_op!(d, ll(r!(s)).wrapping_neg()),
                NegF { d, s } => float_op!(d, -ff(r!(s))),
                NegD { d, s } => double_op!(d, -dd(r!(s))),
                NotI { d, s } => int_op!(d, !ii(r!(s))),
                NotL { d, s } => long_op!(d, !ll(r!(s))),
                Not { d, s } => bool_op!(d, !zz(r!(s))),
                Cmp { op, d, a, b } => bool_op!(d, compare(op, r!(a), r!(b))),
                JCmpFalse { op, a, b, t } => {
                    if !compare(op, r!(a), r!(b)) {
                        ip = unsafe { code.add(t as usize) };
                    }
                }
                JCmp { op, a, b, t, lc } => {
                    if compare(op, r!(a), r!(b)) {
                        if lc && loop_tick(counter) && let Err(e) = self.loop_exhausted(counter) {
                            throw!(e)
                        }
                        ip = unsafe { code.add(t as usize) };
                    }
                }
                IncJCmpK { op, a, inc, k, t, when, lc } => {
                    let v = ii(r!(a)).wrapping_add(inc);
                    int_op!(a, v);
                    if compare_int(op, v, k) == when {
                        if lc && loop_tick(counter) && let Err(e) = self.loop_exhausted(counter) {
                            throw!(e)
                        }
                        ip = unsafe { code.add(t as usize) };
                    }
                }
                JCmpK { op, a, k, t, when, lc } => {
                    if compare_int(op, ii(r!(a)), k) == when {
                        if lc && loop_tick(counter) && let Err(e) = self.loop_exhausted(counter) {
                            throw!(e)
                        }
                        ip = unsafe { code.add(t as usize) };
                    }
                }
                Jmp { t } => ip = unsafe { code.add(t as usize) },
                Jf { c, t } => {
                    if !zz(r!(c)) {
                        ip = unsafe { code.add(t as usize) };
                    }
                }
                Jt { c, t } => {
                    if zz(r!(c)) {
                        ip = unsafe { code.add(t as usize) };
                    }
                }
                JNull { s, t } => {
                    if r!(s).is_null() {
                        ip = unsafe { code.add(t as usize) };
                    }
                }
                JNonNull { s, t } => {
                    if !r!(s).is_null() {
                        ip = unsafe { code.add(t as usize) };
                    }
                }
                Ret { s } => {
                    *pc_ref = pc_now!();
                        // the value stays in its register: `exec` moves it out with a typed read
                    return Ok(Exit::Ret(s));
                }
                RetVoid => {
                    *pc_ref = pc_now!();
                        return Ok(Exit::RetVoid);
                }
                LoopCheck => {
                    if loop_tick(counter) && let Err(e) = self.loop_exhausted(counter) {
                        throw!(e)
                    }
                }
                CallFunc { d, f, a, n } => {
                    *pc_ref = pc_now!();
                        return Ok(Exit::Call { f, d, a, n });
                }
                DocValue { d, field, what } => {
                    // the host cannot call back into the interpreter, so `regs` stays valid.
                    // Numbers and booleans are read straight from the buffer; everything else
                    // (strings, dates, empty fields and their errors) goes through `doc_fast`.
                    let i = field as usize;
                    if self.st.doc_buf_gen[i] != self.st.doc_gen
                        && let Err(e) = crate::es::doc_fetch(self, field)
                    {
                        throw!(e)
                    }
                    let dv = &self.st.doc_bufs[i];
                    match what {
                        DocWhat::Fetch => prim!(d, tag::NULL, 0),
                        DocWhat::Size => int_op!(d, dv.len() as i32),
                        DocWhat::Empty => bool_op!(d, dv.is_empty()),
                        DocWhat::Value => {
                            use crate::host::DocKind as K;
                            match dv.kind {
                                K::Long | K::Integer | K::Short | K::Byte if !dv.longs.is_empty() => long_op!(d, dv.longs[0]),
                                K::Double | K::Float | K::HalfFloat | K::ScaledFloat if !dv.doubles.is_empty() => double_op!(d, dv.doubles[0]),
                                K::Boolean if !dv.longs.is_empty() => bool_op!(d, dv.longs[0] != 0),
                                _ => put_vr!(d, crate::es::doc_fast(self, field, what)),
                            }
                        }
                    }
                }
                LoadVar { d, var } => {
                    let v = &self.st.vars[var as usize];
                    if !v.is_heap() {
                        // SAFETY: as in `r!`; `v` owns no heap data
                        let slot = unsafe { regs.add(d as usize) };
                        unsafe {
                            if (*slot).is_heap() {
                                drop_heap(slot);
                            }
                            copy_prim(slot, v);
                        }
                    } else if let Value::Obj(o) = v
                        && let Obj::Es(crate::es::EsObj::LazyScore) = &**o
                    {
                        let x = self.host.score();
                        double_op!(d, x)
                    } else if !matches!(v, Value::Obj(_)) {
                        let v = v.clone();
                        put!(d, v);
                    } else {
                        put_vr!(d, self.load_var(var))
                    }
                }
                MapGetK { d, r, k, m: _, site } => {
                    // SAFETY: as in `r!`; `map_get_k` neither touches the registers nor runs
                    // script code
                    let recv: &Value = unsafe { &*regs.add(r as usize) };
                    // cache hit: the key still sits at the index this site found it at last time
                    let mut hit = None;
                    if let Value::Map(map) = recv
                        && self.st.map_cache[site as usize].0 == Rc::as_ptr(map) as usize
                        && let Value::Str(key) = &self.st.consts[k as usize]
                    {
                        let data = map.data.borrow();
                        if let Some((kk, v)) = data.m.get_index(self.st.map_cache[site as usize].1 as usize)
                            && let Value::Str(ks) = &kk.0
                            && (Rc::ptr_eq(ks, key) || ks.as_str() == key.as_str())
                            // lazily bound entries (`params._source`) resolve through the host
                            && !(map.lazy_host && matches!(v, Value::Obj(_)))
                        {
                            hit = Some(v.clone());
                        }
                    }
                    match hit {
                        Some(v) => put!(d, v),
                        None => match self.map_get_k(recv, k, site) {
                            Some(res) => put_vr!(d, res),
                            None => slow!(insn),
                        },
                    }
                }
                ToD { d, s, c } => match *r!(s) {
                    Value::Double(x) => double_op!(d, x),
                    Value::Int(x) => double_op!(d, x as f64),
                    Value::Long(x) => double_op!(d, x as f64),
                    Value::Float(x) => double_op!(d, x as f64),
                    _ => {
                        let generic = Cast { d, s, c };
                        slow!(&generic)
                    }
                },
                ToL { d, s, c } => match *r!(s) {
                    Value::Long(x) => long_op!(d, x),
                    Value::Int(x) => long_op!(d, x as i64),
                    _ => {
                        let generic = Cast { d, s, c };
                        slow!(&generic)
                    }
                },
                ToI { d, s, c } => match *r!(s) {
                    Value::Int(x) => int_op!(d, x),
                    _ => {
                        let generic = Cast { d, s, c };
                        slow!(&generic)
                    }
                },
                Cast { d, s, c } => match crate::cast::fast_cast(r!(s), &prog.casts[c as usize]) {
                    Some(v) => put!(d, v),
                    None => slow!(insn),
                },
                DefCmp { op, d, a, b, info } => match crate::defmath::fast_compare(op, r!(a), r!(b)).filter(|_| prog.linked(info, r!(a), r!(b))) {
                    Some(v) => bool_op!(d, v),
                    None => slow!(insn),
                },
                DefBin { op, d, a, b, info } => match crate::defmath::fast_binary(op, r!(a), r!(b), &prog.defops[info as usize]).filter(|_| prog.linked(info, r!(a), r!(b))) {
                    Some(v) => put!(d, v),
                    None => slow!(insn),
                },
                IterNext { d, it, t, p: 0 } => {
                    let next = match r!(it) {
                        Value::Obj(o) => match &**o {
                            Obj::Iterator(i) => Some(i.borrow_mut().next()),
                            _ => None,
                        },
                        _ => None,
                    };
                    match next {
                        Some(Ok(Some(v))) => put!(d, v),
                        Some(Ok(None)) => ip = unsafe { code.add(t as usize) },
                        Some(Err(e)) => throw!(e),
                        None => slow!(insn),
                    }
                }
                _ => slow!(insn),
            }
        }
    }

    /// Instructions that call into library code, the host or other functions.
    #[inline(never)]
    fn slow(&mut self, func: &Function, base: usize, insn: &Insn, pc: &mut usize, counter: &mut u32) -> Result<Option<Exit>, Thrown> {
        use crate::vm::Insn::*;
        let prog: &'a Program = self.prog;
        let reg: &'static Registry = self.reg;
        let _ = counter;
        match *insn {
            Cast { d, s, c } => {
                let v = self.r(base, s).clone();
                let cast = &prog.casts[c as usize];
                if v.is_null()
                    && let crate::ir::Cast::Num { unbox: Some(p), .. } | crate::ir::Cast::Unbox(p) = *cast
                {
                    return Err(crate::cast::unbox_null(p, func.null_desc(*pc - 1)));
                }
                let out = crate::cast::cast(self, v, cast)?;
                self.set(base, d, out);
            }
            DefBin { op, d, a, b, info } => {
                crate::defmath::link(prog, info, op, self.r(base, a), self.r(base, b))?;
                let v = crate::defmath::binary(op, self.r(base, a), self.r(base, b), &prog.defops[info as usize])?;
                self.set(base, d, v);
            }
            DefUn { op, d, s, info } => {
                crate::defmath::link_unary(prog, info, self.r(base, s))?;
                let v = crate::defmath::unary(op, self.r(base, s), &prog.defops[info as usize])?;
                self.set(base, d, v);
            }
            Concat { d, a, n } => {
                let mut s = String::new();
                for i in 0..n {
                    self.r(base, a + i).write_java_string(&mut s, 0);
                    if let Some(t) = crate::guard::take_pending() {
                        return Err(t);
                    }
                    crate::guard::check_string(s.len())?;
                }
                self.set(base, d, Value::string(s));
            }
            DefCmp { op, d, a, b, info } => {
                crate::defmath::link(prog, info, op, self.r(base, a), self.r(base, b))?;
                let v = crate::defmath::compare(op, self.r(base, a), self.r(base, b))?;
                self.set(base, d, Value::Bool(v));
            }
            Throw { s } => {
                let v = self.r(base, s).clone();
                return Err(throw_value(v));
            }
            Call { d, m, a, n } => {
                let mut args = self.take_args(base, a, n);
                let imp = self.virtual_imp(m, &args);
                let res = self.call_native(imp, m, &mut args);
                // a new exception remembers where it was constructed
                if let Ok(Value::Obj(o)) = &res
                    && let Obj::Exception(e) = &**o
                    && e.origin.borrow().is_none()
                    && let Some(&offset) = func.locs.get(*pc - 1)
                {
                    *e.origin.borrow_mut() = Some(Box::new(crate::value::ExceptionOrigin { offset, frames: Vec::new() }));
                }
                self.set_vr(base, d, res)?;
            }
            CallRecv { d, m, a, n } => {
                if self.r(base, a).is_null() {
                    return Err(crate::dynamic::null_receiver(reg, m, func.null_desc(*pc - 1)));
                }
                let mut args = self.take_args(base, a, n);
                let imp = self.virtual_imp(m, &args);
                let res = self.call_native(imp, m, &mut args);
                self.set_vr(base, d, res)?;
            }
            CallDyn { d, name, a, n, site, res } => {
                let mut args = self.take_args(base, a, n);
                let v = crate::dynamic::call(self, name, site, &mut args)?;
                let v = if res != u32::MAX { crate::cast::cast(self, v, &prog.casts[res as usize])? } else { v };
                self.set(base, d, v);
            }
            CallCtx { d, m, a, n, binding, ctor_loc } => {
                self.st.binding_loc = ctor_loc;
                let mut args = self.take_args(base, a, n);
                self.st.cur_binding = binding;
                let imp = reg.method(m).imp;
                let res = imp(self, &mut args);
                self.set_vr(base, d, res)?;
            }
            StaticField { d, f } => {
                let field = reg.field(f);
                let res = (field.get)(self, &mut [Value::Long(field.data)]);
                self.set_vr(base, d, res)?;
            }
            Field { d, r, f } => {
                let recv = self.r(base, r).clone();
                if recv.is_null() {
                    let why = func.null_desc(*pc - 1).map(|d| format!(" because {d} is null")).unwrap_or_default();
                    return Err(npe(format!("Cannot read field \"{}\"{why}", reg.field(f).name)));
                }
                let get = reg.field(f).get;
                let v = get(self, &mut [recv])?;
                self.set(base, d, v);
            }
            GetDot { d, r, name, res } => {
                let recv = self.r(base, r).clone();
                let v = crate::dynamic::get_dot(self, &recv, name)?;
                let v = if res != u32::MAX { crate::cast::cast(self, v, &prog.casts[res as usize])? } else { v };
                self.set(base, d, v);
            }
            SetDot { r, name, v } => {
                let recv = self.r(base, r).clone();
                let val = self.r(base, v).clone();
                crate::dynamic::set_dot(self, &recv, name, val)?;
            }
            ArrLoad { d, r, i, ty } => {
                if self.r(base, r).is_null() {
                    return Err(crate::dynamic::null_array(prog.types[ty as usize], true, func.null_desc(*pc - 1)));
                }
                let v = crate::dynamic::array_load(self.r(base, r), self.r(base, i), func.null_desc(*pc - 1))?;
                self.set(base, d, v);
            }
            ArrStore { r, i, v, ty } => {
                if self.r(base, r).is_null() {
                    return Err(crate::dynamic::null_array(prog.types[ty as usize], false, func.null_desc(*pc - 1)));
                }
                let val = self.r(base, v).clone();
                crate::dynamic::array_store(self.r(base, r), self.r(base, i), val, func.null_desc(*pc - 1))?;
            }
            ArrLen { d, r } => {
                let v = crate::dynamic::array_len(self.r(base, r), func.null_desc(*pc - 1))?;
                self.set(base, d, Value::Int(v));
            }
            Flip { d, r, i, list } => {
                let v = crate::dynamic::flip(self.r(base, r), self.r(base, i), list, func.null_desc(*pc - 1))?;
                self.set(base, d, Value::Int(v));
            }
            GetIdx { d, r, i, res } => {
                let recv = self.r(base, r).clone();
                let idx = self.r(base, i).clone();
                let v = crate::dynamic::get_index(self, &recv, &idx)?;
                let v = if res != u32::MAX { crate::cast::cast(self, v, &prog.casts[res as usize])? } else { v };
                self.set(base, d, v);
            }
            SetIdx { r, i, v, vty } => {
                let recv = self.r(base, r).clone();
                let idx = self.r(base, i).clone();
                let val = self.r(base, v).clone();
                let vty = if vty == u32::MAX { None } else { Some(prog.types[vty as usize]) };
                crate::dynamic::set_index(self, &recv, &idx, val, vty)?;
            }
            DefNorm { d, r, i, t } => {
                let recv = self.r(base, r).clone();
                let idx = self.r(base, i).clone();
                if t != u32::MAX {
                    crate::dynamic::check_index_site(self, &recv, prog.types[t as usize])?;
                }
                let v = crate::dynamic::normalize_def(self, &recv, &idx)?;
                self.set(base, d, v);
            }
            NewList { d, a, n } => {
                let start = base + a as usize;
                let items: Vec<Value> = (0..n as usize).map(|i| std::mem::take(&mut self.stack[start + i])).collect();
                self.set(base, d, JList::array_list(items));
            }
            NewMap { d, a, n } => {
                let start = base + a as usize;
                let m = JMap::hash_map();
                for i in 0..n as usize {
                    let k = std::mem::take(&mut self.stack[start + 2 * i]);
                    let v = std::mem::take(&mut self.stack[start + 2 * i + 1]);
                    m.insert_raw(k, v);
                }
                self.set(base, d, Value::Map(Rc::new(m)));
            }
            NewArray { d, ty, a, n } => {
                let t = prog.types[ty as usize];
                let dims: Vec<i64> = (0..n).map(|i| self.r(base, a + i as u16).as_i64().unwrap_or(0)).collect();
                let v = crate::dynamic::new_array(t, &dims)?;
                self.set(base, d, v);
            }
            ArrayInit { d, ty, a, n } => {
                let t = prog.types[ty as usize];
                let start = base + a as usize;
                let items: Vec<Value> = (0..n as usize).map(|i| std::mem::take(&mut self.stack[start + i])).collect();
                let v = crate::dynamic::array_from(t, items);
                self.set(base, d, v);
            }
            InstanceOf { d, s, ty } => {
                let t = prog.types[ty as usize];
                let v = Value::Bool(reg.instance_of(self.r(base, s), t));
                self.set(base, d, v);
            }
            Lambda { d, f, a, n, iface } => {
                let start = base + a as usize;
                let caps: Vec<Value> = (0..n as usize).map(|i| self.stack[start + i].clone()).collect();
                let iface = if iface == u32::MAX { None } else { Some(ClassId(iface as u16)) };
                let v = crate::func::lambda(f, caps, iface);
                self.set(base, d, v);
            }
            FuncRef { d, spec, cap } => {
                let c = if cap == Reg::MAX { Value::Null } else { self.r(base, cap).clone() };
                let v = crate::func::make_ref(self, spec, c)?;
                self.set(base, d, v);
            }
            Pattern { d, p } => {
                let v = self.st.patterns[p as usize].clone();
                self.set(base, d, v);
            }
            RegexOp { find, d, s, p } => {
                let sv = self.r(base, s).clone();
                let pv = self.r(base, p).clone();
                let v = crate::regex::regex_op(self, find, &sv, &pv)?;
                self.set(base, d, Value::Bool(v));
            }
            IterInit { d, s } => {
                let v = self.r(base, s).clone();
                if let Value::List(l) = &v {
                    crate::lib_coll::view_enter(l, &crate::lib_coll::ViewCall { owner: cls::LIST, name: "iterator", arity: 0 })?;
                }
                let it = crate::iter::iterator_of(self, &v)?;
                self.set(base, d, it);
            }
            Alloc { bytes } => self.charge(bytes as i64)?,
            AllocArray { a, n, w } => {
                // `AllocSizes.arrayBytes(mulSat(dims...), w)`
                let mut count: i64 = 1;
                for i in 0..n {
                    let d = self.r(base, a + i as u16).as_i64().unwrap_or(0);
                    count = count.checked_mul(d).unwrap_or(if (count < 0) != (d < 0) { i64::MIN } else { i64::MAX });
                }
                let bytes = (w as i64).checked_mul(count).unwrap_or(if count < 0 { i64::MIN } else { i64::MAX });
                let bytes = bytes.saturating_add(16).saturating_add(7) & !7;
                self.charge(bytes)?;
            }
            AllocConcat { a, n, info } => {
                let (fixed, refs) = &prog.concat_infos[info as usize];
                let mut bytes = *fixed as i64;
                for i in 0..n {
                    if refs[i as usize] {
                        bytes += match self.r(base, a + i) {
                            Value::Null => 8,
                            Value::Str(s) => s.len16() as i64 * 2,
                            _ => 256,
                        };
                    }
                }
                self.charge(bytes)?;
            }
            DocAt { d, field, i, p } => {
                let idx = self.r(base, i).as_i64().unwrap_or(0) as usize;
                let v = crate::es::doc::doc_at(self, field, idx)?;
                let v = if p == 0 { v } else { crate::iter::value_iterator_convert(v, crate::ir::Prim::from_index(p - 1))? };
                self.set(base, d, v);
            }
            IterNext { d, it, t, p } => {
                let itv = self.r(base, it).clone();
                let n = if p == 0 { crate::iter::next(self, &itv)? } else { crate::iter::next_prim(self, &itv, crate::ir::Prim::from_index(p - 1))? };
                match n {
                    Some(v) => self.set(base, d, v),
                    None => *pc = t as usize,
                }
            }
            MapGetK { d, r, k, m, .. } => {
                if self.r(base, r).is_null() {
                    return Err(crate::dynamic::null_receiver(reg, m, func.null_desc(*pc - 1)));
                }
                let mut args = crate::vm::smallvec_args::Args::new();
                args.push(self.r(base, r).clone());
                args.push(self.st.consts[k as usize].clone());
                let imp = self.virtual_imp(m, &args);
                let res = imp(self, &mut args);
                self.set_vr(base, d, res)?;
            }
            _ => unreachable!("fast-path instruction in the slow path"),
        }
        Ok(None)
    }

    /// `MapGetK` on a plain hash or insertion-ordered map; `None` for everything else (sorted
    /// and Elasticsearch maps, null receivers), which takes the generic `Map.get` call.
    #[inline]
    fn map_get_k(&mut self, recv: &Value, k: u32, site: u32) -> Option<VR> {
        if let Value::Obj(o) = recv
            && let Obj::Es(crate::es::EsObj::Doc) = &**o
        {
            let key = self.st.consts[k as usize].clone();
            return Some(crate::es::doc::doc_get(self, &key));
        }
        let Value::Map(map) = recv else { return None };
        if map.order() == crate::value::MapOrder::Sorted {
            return None;
        }
        let Value::Str(key) = &self.st.consts[k as usize] else { return None };
        let id = Rc::as_ptr(map) as usize;
        let data = map.data.borrow();
        let cache = &mut self.st.map_cache[site as usize];
        let hit = if cache.0 == id {
            data.m.get_index(cache.1 as usize).filter(|(kk, _)| matches!(&kk.0, Value::Str(s) if Rc::ptr_eq(s, key) || s.as_str() == key.as_str()))
        } else {
            None
        };
        let found = match hit {
            Some((_, v)) => Some(v.clone()),
            None => match data.m.get_full(&crate::value::Key(Value::Str(key.clone()))) {
                Some((i, _, v)) => {
                    *cache = (id, i as u32);
                    Some(v.clone())
                }
                None => None,
            },
        };
        drop(data);
        Some(match found {
            Some(v) if map.lazy_host => crate::es::resolve_param(self, v),
            Some(v) => Ok(v),
            None => Ok(Value::Null),
        })
    }

    /// Value of a context variable at function entry.
    pub fn load_var(&mut self, var: u32) -> VR {
        let v = self.st.vars[var as usize].clone();
        if let Value::Obj(o) = &v
            && let Obj::Es(e) = &**o
                && let Some(x) = crate::es::load_lazy(self, e) {
                    return Ok(x);
                }
        Ok(v)
    }
}

#[inline(always)]
fn ii(v: &Value) -> i32 {
    if let Value::Int(i) = *v { i } else { ii_slow(v) }
}
#[cold]
fn ii_slow(v: &Value) -> i32 {
    match *v {
        Value::Byte(i) => i as i32,
        Value::Short(i) => i as i32,
        Value::Char(i) => i as i32,
        _ => 0,
    }
}
#[inline(always)]
fn ll(v: &Value) -> i64 {
    match *v {
        Value::Long(i) => i,
        Value::Int(i) => i as i64,
        _ => 0,
    }
}
#[inline(always)]
fn ff(v: &Value) -> f32 {
    if let Value::Float(f) = *v { f } else { 0.0 }
}
#[inline(always)]
fn dd(v: &Value) -> f64 {
    if let Value::Double(f) = *v { f } else { 0.0 }
}
#[inline(always)]
fn zz(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

/// Register writes and store-to-load forwarding.
///
/// `Value` is `#[repr(u64)]`: an 8-byte tag at offset 0 and the payload at offset 8. Registers
/// are written as two 8-byte stores (tag, zero-extended payload) and read with loads that each
/// fall inside one of them — the tag word, the payload at its own width, or (for copies) the two
/// words separately — so every load forwards from the store buffer. Rust's default enum code
/// mixes widths (a 4-byte payload store, later a 16-byte copy of the whole value), and each
/// such load that spans several stores costs a forwarding stall (~12 cycles).
mod tag {
    pub const NULL: u64 = 0;
    pub const BOOL: u64 = 1;
    pub const BYTE: u64 = 2;
    pub const SHORT: u64 = 3;
    pub const CHAR: u64 = 4;
    pub const INT: u64 = 5;
    pub const LONG: u64 = 6;
    pub const FLOAT: u64 = 7;
    pub const DOUBLE: u64 = 8;
}

/// Writes a primitive value (`tag`, payload bits) into an uninitialized register.
///
/// # Safety
/// `slot` must be valid for writes; `tag` must be a primitive tag and `bits` a valid payload
/// for it (zero-extended).
#[inline(always)]
unsafe fn store_prim(slot: *mut Value, tag: u64, bits: u64) {
    // two 8-byte stores (volatile so LLVM cannot fuse them into one vector store, whose
    // construction would add latency to every loop-carried register)
    unsafe {
        let p = slot as *mut u64;
        p.write_volatile(tag);
        p.add(1).write_volatile(bits);
    }
}

/// Copies a non-heap value between registers word by word: each 8-byte load is contained in
/// one earlier store, so it forwards (a 16-byte load spanning two stores would stall).
///
/// # Safety
/// `src` must not own heap data (`!is_heap()`); `dst` must be valid for writes and hold no heap
/// data either (or have been dropped).
#[inline(always)]
unsafe fn copy_prim(dst: *mut Value, src: *const Value) {
    use std::mem::MaybeUninit;
    unsafe {
        let s = src as *const MaybeUninit<u64>;
        let d = dst as *mut MaybeUninit<u64>;
        d.write_volatile(s.read_volatile());
        d.add(1).write_volatile(s.add(1).read_volatile());
    }
}

/// Moves `*src` into an uninitialized register, reading its tag and payload separately (the
/// caller must forget `src`). Values returned by out-of-line calls sit in memory the callee
/// wrote as separate tag and payload stores; copying them whole would stall.
///
/// # Safety
/// `slot` must be valid for writes, and ownership of `*src`'s heap data moves to `slot`.
#[inline(always)]
unsafe fn move_value(slot: *mut Value, src: &Value) {
    unsafe {
        match *src {
            Value::Double(x) => store_prim(slot, tag::DOUBLE, x.to_bits()),
            Value::Int(x) => store_prim(slot, tag::INT, x as u32 as u64),
            Value::Long(x) => store_prim(slot, tag::LONG, x as u64),
            Value::Bool(x) => store_prim(slot, tag::BOOL, x as u64),
            Value::Null => store_prim(slot, tag::NULL, 0),
            Value::Float(x) => store_prim(slot, tag::FLOAT, x.to_bits() as u64),
            Value::Byte(x) => store_prim(slot, tag::BYTE, x as u8 as u64),
            Value::Short(x) => store_prim(slot, tag::SHORT, x as u16 as u64),
            Value::Char(x) => store_prim(slot, tag::CHAR, x as u64),
            // heap variants: the tag word and the pointer word, both fully initialized
            _ => {
                let s = src as *const Value as *const u64;
                let d = slot as *mut u64;
                d.write(s.read());
                (d.add(1) as *mut *const u8).write((s.add(1) as *const *const u8).read());
            }
        }
    }
}

/// Stores a value into an uninitialized register (see [`store_prim`]).
///
/// # Safety
/// `slot` must be valid for writes.
#[inline(always)]
unsafe fn write_value(slot: *mut Value, v: Value) {
    // `v` usually arrives in memory written by the callee as separate tag and payload stores;
    // matching on it in place reads them separately (each load forwards), where moving it first
    // would copy it with one 16-byte load that stalls. The primitive arms `forget` it: dropping
    // would be an out-of-line drop-glue call (LLVM does not see it is a no-op).
    unsafe {
        match v {
            Value::Double(x) => store_prim(slot, tag::DOUBLE, x.to_bits()),
            Value::Int(x) => store_prim(slot, tag::INT, x as u32 as u64),
            Value::Long(x) => store_prim(slot, tag::LONG, x as u64),
            Value::Bool(x) => store_prim(slot, tag::BOOL, x as u64),
            Value::Null => store_prim(slot, tag::NULL, 0),
            Value::Float(x) => store_prim(slot, tag::FLOAT, x.to_bits() as u64),
            Value::Byte(x) => store_prim(slot, tag::BYTE, x as u8 as u64),
            Value::Short(x) => store_prim(slot, tag::SHORT, x as u16 as u64),
            Value::Char(x) => store_prim(slot, tag::CHAR, x as u64),
            other => {
                std::ptr::write(slot, other);
                return;
            }
        }
    }
    #[allow(clippy::forget_non_drop)]
    std::mem::forget(v);
}

/// Moves a value out of a register. Primitives are read by type and rebuilt: a whole-value copy
/// of one written by narrower stores would stall store-to-load forwarding.
#[inline(always)]
fn take_reg(slot: &mut Value) -> Value {
    match *slot {
        Value::Double(x) => Value::Double(x),
        Value::Int(x) => Value::Int(x),
        Value::Long(x) => Value::Long(x),
        Value::Bool(x) => Value::Bool(x),
        Value::Null => Value::Null,
        _ => std::mem::take(slot),
    }
}

/// Appends `n` null registers (cheaper than `Vec::resize`, which clones the fill value).
#[inline(always)]
fn push_frame(stack: &mut Vec<Value>, n: usize) {
    stack.reserve(n);
    let len = stack.len();
    // SAFETY: capacity was reserved; every new slot is initialized before `set_len`
    unsafe {
        let p = stack.as_mut_ptr().add(len);
        for i in 0..n {
            store_prim(p.add(i), tag::NULL, 0);
        }
        stack.set_len(len + n);
    }
}

/// A library call's result, or the stack overflow a recursion inside it ran into (see
/// [`crate::guard::take_pending`]).
#[inline(always)]
fn overflowed(r: VR) -> VR {
    if let Some(t) = crate::guard::take_pending() {
        return Err(t);
    }
    r
}

/// Drops the registers above `base`, touching only those that own heap data.
#[inline(always)]
fn pop_frame(stack: &mut Vec<Value>, base: usize) {
    let len = stack.len();
    if base >= len {
        return;
    }
    // SAFETY: the slots in `base..len` are initialized; the length is cut first so a panicking
    // drop cannot drop a slot twice
    unsafe {
        stack.set_len(base);
        let p = stack.as_mut_ptr();
        for i in base..len {
            let slot = p.add(i);
            if (*slot).is_heap() {
                std::ptr::drop_in_place(slot);
            }
        }
    }
}

/// Drops a register's heap value (kept out of line: the dispatch loop only pays a tag check).
///
/// # Safety
/// `slot` must point to an initialized register, which is left logically uninitialized.
#[cold]
#[inline(never)]
unsafe fn drop_heap(slot: *mut Value) {
    unsafe { std::ptr::drop_in_place(slot) }
}

/// Iterations between cancellation polls (Elasticsearch's `CANCELLATION_POLL_INTERVAL`).
pub const CANCELLATION_POLL_INTERVAL: u32 = 1000;

/// Counts one loop iteration; true when the frame's counter ran out (see
/// [`Vm::loop_exhausted`]).
#[inline(always)]
fn loop_tick(counter: &mut u32) -> bool {
    if *counter > 0 {
        *counter -= 1;
        return *counter == 0;
    }
    false
}

#[inline]
fn compare_int(op: CmpOp, a: i32, b: i32) -> bool {
    use CmpOp::*;
    match op {
        EqI => a == b,
        NeI => a != b,
        LtI => a < b,
        LeI => a <= b,
        GtI => a > b,
        GeI => a >= b,
        _ => false,
    }
}

fn div_zero() -> Thrown {
    Throw::new(cls::ARITHMETIC_EXCEPTION, Some("/ by zero".to_string()))
}

#[inline]
fn compare(op: CmpOp, a: &Value, b: &Value) -> bool {
    use CmpOp::*;
    match op {
        EqI => ii(a) == ii(b),
        NeI => ii(a) != ii(b),
        LtI => ii(a) < ii(b),
        LeI => ii(a) <= ii(b),
        GtI => ii(a) > ii(b),
        GeI => ii(a) >= ii(b),
        EqL => ll(a) == ll(b),
        NeL => ll(a) != ll(b),
        LtL => ll(a) < ll(b),
        LeL => ll(a) <= ll(b),
        GtL => ll(a) > ll(b),
        GeL => ll(a) >= ll(b),
        EqF => ff(a) == ff(b),
        NeF => ff(a) != ff(b),
        LtF => ff(a) < ff(b),
        LeF => ff(a) <= ff(b),
        GtF => ff(a) > ff(b),
        GeF => ff(a) >= ff(b),
        EqD => dd(a) == dd(b),
        NeD => dd(a) != dd(b),
        LtD => dd(a) < dd(b),
        LeD => dd(a) <= dd(b),
        GtD => dd(a) > dd(b),
        GeD => dd(a) >= dd(b),
        EqZ => zz(a) == zz(b),
        NeZ => zz(a) != zz(b),
        EqObj => a.java_equals(b),
        NeObj => !a.java_equals(b),
        EqRef => same_ref(a, b),
        NeRef => !same_ref(a, b),
    }
}

pub fn same_ref_pub(a: &Value, b: &Value) -> bool {
    same_ref(a, b)
}

/// Reference identity. Boxed numbers compare by value the way small cached boxes do.
fn same_ref(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Str(x), Value::Str(y)) => Rc::ptr_eq(x, y),
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y),
        (Value::Map(x), Value::Map(y)) | (Value::Set(x), Value::Set(y)) => Rc::ptr_eq(x, y),
        (Value::Obj(x), Value::Obj(y)) => Rc::ptr_eq(x, y),
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y && (-128..=127).contains(x),
        (Value::Long(x), Value::Long(y)) => x == y && (-128..=127).contains(x),
        (Value::Short(x), Value::Short(y)) => x == y && (-128..=127).contains(x),
        (Value::Byte(x), Value::Byte(y)) => x == y,
        (Value::Char(x), Value::Char(y)) => x == y && *x <= 127,
        _ => false,
    }
}

/// The value a `catch` binds: the thrown object.
fn exception_value(t: Throw) -> Value {
    if let Some(obj) = t.object
        && let Ok(v) = obj.downcast::<SendValue>() {
            return v.0;
        }
    let origin = t.offset.map(|offset| Box::new(crate::value::ExceptionOrigin { offset, frames: t.frames }));
    Value::obj(Obj::Exception(JException { class: t.class, message: t.message, origin: std::cell::RefCell::new(origin), error_offset: t.error_offset }))
}

/// A thrown script value carried inside a [`Throw`].
struct SendValue(Value);

/// `throw x;`
pub fn throw_value(v: Value) -> Thrown {
    match &v {
        Value::Null => npe("Cannot throw exception because \"<local1>\" is null"),
        Value::Obj(o) => match &**o {
            Obj::Exception(e) => {
                let mut t = Throw::new(e.class, e.message.clone());
                t.error_offset = e.error_offset;
                if let Some(o) = &*e.origin.borrow() {
                    t.offset = Some(o.offset);
                    t.frames = o.frames.clone();
                }
                t.object = Some(Box::new(SendValue(v.clone())));
                t
            }
            _ => cce("cannot throw a non-exception value"),
        },
        _ => cce("cannot throw a non-exception value"),
    }
}

pub mod smallvec_args {
    //! Call arguments moved out of their registers: up to [`INLINE`] live in place (only the
    //! initialized ones are ever touched — calls are hot, and writing then dropping a full
    //! array of nulls per call showed up in profiles), more spill to a vector.
    use std::mem::MaybeUninit;

    use crate::value::Value;

    const INLINE: usize = 6;

    pub struct Args {
        len: usize,
        inline: [MaybeUninit<Value>; INLINE],
        spill: Vec<Value>,
    }

    impl Args {
        #[inline(always)]
        pub fn new() -> Args {
            Args { len: 0, inline: [const { MaybeUninit::uninit() }; INLINE], spill: Vec::new() }
        }
        #[inline(always)]
        pub fn push(&mut self, v: Value) {
            if self.len < INLINE {
                self.inline[self.len].write(v);
                self.len += 1;
            } else {
                if self.spill.is_empty() {
                    // SAFETY: the first `len` inline slots are initialized; they move to the
                    // vector and `len` stays as the total count, with the inline slots no
                    // longer considered live (`spilled` below)
                    self.spill.extend(self.inline.iter().take(self.len).map(|x| unsafe { x.assume_init_read() }));
                }
                self.spill.push(v);
                self.len += 1;
            }
        }
        /// Moves the value out of a register (leaving `null`), field by field.
        ///
        /// # Safety
        /// `src` must point to an initialized register.
        #[inline(always)]
        pub unsafe fn take_from(&mut self, src: *mut Value) {
            if self.len < INLINE {
                unsafe {
                    super::move_value(self.inline[self.len].as_mut_ptr(), &*src);
                    super::store_prim(src, super::tag::NULL, 0);
                }
                self.len += 1;
            } else {
                // SAFETY: as documented
                let v = unsafe { std::mem::take(&mut *src) };
                self.push(v);
            }
        }
        #[inline(always)]
        fn spilled(&self) -> bool {
            self.len > INLINE
        }
    }

    impl Drop for Args {
        #[inline(always)]
        fn drop(&mut self) {
            if !self.spilled() {
                for x in &mut self.inline[..self.len] {
                    // SAFETY: the first `len` slots are initialized and dropped once
                    unsafe { x.assume_init_drop() };
                }
            }
        }
    }

    impl Default for Args {
        fn default() -> Self {
            Self::new()
        }
    }

    impl std::ops::Deref for Args {
        type Target = [Value];
        #[inline(always)]
        fn deref(&self) -> &[Value] {
            if self.spilled() {
                &self.spill
            } else {
                // SAFETY: the first `len` slots are initialized
                unsafe { &*(&self.inline[..self.len] as *const [MaybeUninit<Value>] as *const [Value]) }
            }
        }
    }
    impl std::ops::DerefMut for Args {
        #[inline(always)]
        fn deref_mut(&mut self) -> &mut [Value] {
            if self.spilled() {
                &mut self.spill
            } else {
                // SAFETY: the first `len` slots are initialized
                unsafe { &mut *(&mut self.inline[..self.len] as *mut [MaybeUninit<Value>] as *mut [Value]) }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn inline_and_spilled_arguments() {
            for n in [0usize, 1, 5, 6, 7, 12] {
                let mut a = Args::new();
                for i in 0..n {
                    a.push(Value::string(format!("v{i}")));
                }
                assert_eq!(a.len(), n);
                for (i, v) in a.iter().enumerate() {
                    assert_eq!(v.to_java_string(), format!("v{i}"));
                }
                a[0..n.min(1)].iter_mut().for_each(|v| *v = Value::Int(1));
            }
        }
    }
}

#[cfg(test)]
mod size_tests {
    use super::{store_prim, tag, Value};

    #[test]
    fn value_layout_matches_store_prim() {
        // `store_prim` relies on `#[repr(u64)]`: tag at offset 0, payload at offset 8
        fn raw(v: &Value) -> [u64; 2] {
            let mut out = [0u64; 2];
            // SAFETY: reads the tag and the first payload word; the payloads below are 8 bytes
            // or zero-extended by `store_prim`, so compare only what each variant defines
            unsafe { std::ptr::copy_nonoverlapping(v as *const Value as *const u64, out.as_mut_ptr(), 1) };
            out
        }
        assert_eq!(std::mem::size_of::<Value>(), 16);
        // instructions stay small: the dispatch loop streams them
        assert_eq!(std::mem::size_of::<super::Insn>(), 20);
        for (v, t) in [(Value::Null, tag::NULL), (Value::Bool(true), tag::BOOL), (Value::Byte(-1), tag::BYTE), (Value::Short(-1), tag::SHORT), (Value::Char(1), tag::CHAR), (Value::Int(-1), tag::INT), (Value::Long(-1), tag::LONG), (Value::Float(1.5), tag::FLOAT), (Value::Double(1.5), tag::DOUBLE)] {
            assert_eq!(raw(&v)[0], t, "{v:?}");
        }
        let mut slot = std::mem::MaybeUninit::<Value>::uninit();
        for (t, bits, want) in [(tag::INT, (-7i32) as u32 as u64, Value::Int(-7)), (tag::DOUBLE, 2.5f64.to_bits(), Value::Double(2.5)), (tag::BYTE, (-3i8) as u8 as u64, Value::Byte(-3)), (tag::SHORT, (-300i16) as u16 as u64, Value::Short(-300)), (tag::CHAR, 0xFFFF, Value::Char(0xFFFF)), (tag::FLOAT, (-0.5f32).to_bits() as u64, Value::Float(-0.5)), (tag::BOOL, 1, Value::Bool(true)), (tag::LONG, u64::MAX, Value::Long(-1)), (tag::NULL, 0, Value::Null)] {
            // SAFETY: valid tag/payload pairs
            unsafe { store_prim(slot.as_mut_ptr(), t, bits) };
            let got = unsafe { slot.assume_init_ref() };
            assert_eq!(format!("{got:?}/{}", got.class().0), format!("{want:?}/{}", want.class().0));
        }
    }

    #[test]
    fn sizes() {
        eprintln!("SIZES value={} insn={} thrown_result={}", std::mem::size_of::<crate::value::Value>(), std::mem::size_of::<super::Insn>(), std::mem::size_of::<super::VR>());
    }
}
