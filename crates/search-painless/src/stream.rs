//! `java.util.stream`: `Stream`, `IntStream`, `LongStream`, `DoubleStream`, their builders, and
//! `Collectors`.
//!
//! A stream is a source plus a list of pending stages; intermediate operations move the pipeline
//! into a new stream object (the old one becomes "linked", so reusing it fails like Java's) and
//! terminal operations run it. Running pushes each source element through the stages, which
//! keeps Java's laziness where scripts can see it: `peek` and `map` run interleaved per element,
//! `limit`, `findFirst` and the `*Match` operations stop the source early, and `sorted`/`distinct`
//! act as barriers.
//!
//! An `ArrayList` (and `HashSet`) source binds late, as the JDK's spliterators do: the elements
//! are read when the terminal operation runs, and a structural change of the collection during
//! the traversal fails it with a `ConcurrentModificationException`. Failures raised by the
//! stream machinery itself (a collector's null checks, natural-order comparisons in `sorted`
//! and `max`, array stores of `toArray`, reuse of a consumed stream) carry the JDK frames of
//! the pipeline that raised them: the terminal operation's [`TermInfo`] (set for the run by
//! [`traced`]) plus the sink chain, barrier and source spliterator frames [`Run`] derives from
//! the stages; shapes it does not model get no frames.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::func::{self, call, call_bool};
use crate::lib_lang::{double, int, long};
use crate::lookup::Builder;
use crate::types::{cls, ClassId, Ty};
use crate::value::{JList, JMap, Key, Obj, Value};
use crate::vm::{iae, npe, Throw, Thrown, Vm, VR};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Ref,
    Int,
    Long,
    Double,
}

impl Kind {
    fn class(self) -> ClassId {
        match self {
            Kind::Ref => cls::REFERENCE_PIPELINE,
            Kind::Int => cls::INT_PIPELINE,
            Kind::Long => cls::LONG_PIPELINE,
            Kind::Double => cls::DOUBLE_PIPELINE,
        }
    }
    fn of_class(c: ClassId) -> Kind {
        match c {
            cls::INT_PIPELINE => Kind::Int,
            cls::LONG_PIPELINE => Kind::Long,
            cls::DOUBLE_PIPELINE => Kind::Double,
            _ => Kind::Ref,
        }
    }
    fn conv(self, v: Value) -> Value {
        match self {
            Kind::Ref => v,
            Kind::Int => Value::Int(int(&v)),
            Kind::Long => Value::Long(long(&v)),
            Kind::Double => Value::Double(double(&v)),
        }
    }
}

pub enum Source {
    /// Elements of a `SIZED` spliterator (collections, arrays, ranges, builders), and the
    /// spliterator they came from (for the JDK frames of failures).
    Items(Vec<Value>, Origin),
    /// An `ArrayList`'s late-binding spliterator: the elements are read when the terminal
    /// operation runs, and a structural change of the list during the traversal fails it with
    /// a `ConcurrentModificationException`.
    List(Rc<JList>),
    /// A `HashSet`'s late-binding key spliterator (fails like [`Source::List`]).
    Keys(Rc<JMap>),
    /// Elements of a spliterator without a known size (`Pattern.splitAsStream`).
    Unsized(Vec<Value>),
    Concat(Box<StreamState>, Box<StreamState>),
    /// `Pattern.splitAsStream`: split when the terminal operation runs.
    Split(std::sync::Arc<crate::regex::Regex>, String),
    /// What is left of a pipeline a terminal or intermediate operation consumed; `head`: it had
    /// no stages (Java's `Head`, whose `forEach` reuse fails elsewhere).
    Consumed { head: bool },
}

pub enum Stage {
    Filter(Value),
    Map(Value, Kind),
    FlatMap(Value),
    Peek(Value),
    Distinct,
    Sorted(Option<Value>),
    Limit(i64),
    Skip(i64),
    /// Primitive widening (`asLongStream`, `boxed`, …).
    Convert(Kind),
}

pub enum StreamState {
    Pipeline { kind: Kind, source: Source, stages: Vec<Stage>, used: bool },
    /// `Stream.builder()`: elements, then built.
    Builder { kind: Kind, items: Vec<Value>, built: bool },
}

impl StreamState {
    pub fn class(&self) -> ClassId {
        match self {
            StreamState::Pipeline { kind, .. } => kind.class(),
            StreamState::Builder { kind, .. } => match kind {
                Kind::Ref => cls::STREAM_BUILDER_IMPL,
                Kind::Int => cls::INT_STREAM_BUILDER_IMPL,
                Kind::Long => cls::LONG_STREAM_BUILDER_IMPL,
                Kind::Double => cls::DOUBLE_STREAM_BUILDER_IMPL,
            },
        }
    }
}

/// A collector definition (`Collectors.*`, `Collector.of`).
pub enum CollectorDef {
    ToList,
    ToSet,
    ToMap { k: Value, v: Value, merge: Option<Value>, supplier: Option<Value> },
    Joining { delim: String, prefix: String, suffix: String },
    GroupingBy { classifier: Value, factory: Option<Value>, down: Rc<CollectorDef> },
    PartitioningBy { pred: Value, down: Rc<CollectorDef> },
    Counting,
    Summing(Kind, Value),
    Averaging(Kind, Value),
    Summarizing(Kind, Value),
    Mapping(Value, Rc<CollectorDef>),
    MinMax { cmp: Value, max: bool },
    Reducing { identity: Option<Value>, mapper: Option<Value>, op: Value },
    ToCollection(Value),
    AndThen(Rc<CollectorDef>, Value),
    Custom { supplier: Value, accumulator: Value, combiner: Value, finisher: Option<Value> },
}

/// The spliterator a stream's source elements come from: it decides the innermost JDK frames
/// of a failure during the traversal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    Unknown,
    ArrayList,
    SubList,
    LinkedList,
    ArrayDeque,
    /// `HashMap.KeySpliterator`: `HashSet`, `HashMap.keySet()`.
    HashKeys,
    HashValues,
    HashEntries,
    /// `Spliterators.IteratorSpliterator` over a linked hash iterator.
    LinkedHash,
    SingletonList,
}

impl Origin {
    /// The origin of `collection.stream()`.
    pub fn of(v: &Value) -> Origin {
        match v {
            Value::List(l) => match l.class {
                cls::ARRAY_LIST | cls::UNMODIFIABLE_LIST => Origin::ArrayList,
                cls::ARRAY_DEQUE => Origin::ArrayDeque,
                cls::SUB_LIST => Origin::SubList,
                cls::LINKED_LIST => Origin::LinkedList,
                cls::SINGLETON_LIST => Origin::SingletonList,
                _ => Origin::Unknown,
            },
            Value::Set(s) => match s.class {
                cls::HASH_SET => Origin::HashKeys,
                cls::LINKED_HASH_SET => Origin::LinkedHash,
                _ => Origin::Unknown,
            },
            Value::Obj(o) => match &**o {
                Obj::MapView { map, kind } if map.class == cls::HASH_MAP => match kind {
                    crate::value::ViewKind::Keys => Origin::HashKeys,
                    crate::value::ViewKind::Values => Origin::HashValues,
                    crate::value::ViewKind::Entries => Origin::HashEntries,
                },
                _ if v.class() == cls::ARRAY_DEQUE => Origin::ArrayDeque,
                _ => Origin::Unknown,
            },
            _ => Origin::Unknown,
        }
    }

    /// `forEachRemaining` of the source spliterator, innermost first.
    fn each(self) -> Option<&'static [&'static str]> {
        Some(match self {
            Origin::Unknown => return None,
            Origin::ArrayList => &["java.util.ArrayList$ArrayListSpliterator.forEachRemaining(ArrayList.java:1724)"],
            Origin::SubList => &["java.util.ArrayList$SubList$2.forEachRemaining(ArrayList.java:1575)"],
            Origin::LinkedList => &["java.util.LinkedList$LLSpliterator.forEachRemaining(LinkedList.java:1250)"],
            Origin::ArrayDeque => &["java.util.ArrayDeque$DeqSpliterator.forEachRemaining(ArrayDeque.java:846)"],
            Origin::HashKeys => &["java.util.HashMap$KeySpliterator.forEachRemaining(HashMap.java:1741)"],
            Origin::HashValues => &["java.util.HashMap$ValueSpliterator.forEachRemaining(HashMap.java:1813)"],
            Origin::HashEntries => &["java.util.HashMap$EntrySpliterator.forEachRemaining(HashMap.java:1884)"],
            Origin::LinkedHash => &["java.util.Iterator.forEachRemaining(Iterator.java:133)", "java.util.Spliterators$IteratorSpliterator.forEachRemaining(Spliterators.java:1939)"],
            Origin::SingletonList => &["java.util.Collections$2.tryAdvance(Collections.java:5189)", "java.util.Collections$2.forEachRemaining(Collections.java:5197)"],
        })
    }

    /// `tryAdvance` of the source spliterator (short-circuiting pipelines).
    fn advance(self) -> Option<&'static str> {
        match self {
            Origin::ArrayList => Some("java.util.ArrayList$ArrayListSpliterator.tryAdvance(ArrayList.java:1701)"),
            _ => None,
        }
    }
}

pub fn stream_value(s: StreamState) -> Value {
    Value::obj(Obj::Stream(RefCell::new(s)))
}

/// A `Stream` over the given elements.
pub fn stream_of(items: Vec<Value>) -> Value {
    stream_value(StreamState::Pipeline { kind: Kind::Ref, source: Source::Items(items, Origin::Unknown), stages: Vec::new(), used: false })
}

/// `Collection.stream()`: an `ArrayList` streams late-bound (see [`Source::List`]), other
/// collections their current elements.
pub fn collection_stream(recv: &Value, items: impl FnOnce() -> Result<Vec<Value>, Thrown>) -> VR {
    let source = match recv {
        Value::List(l) if l.class == cls::ARRAY_LIST && l.view.is_none() => Source::List(l.clone()),
        Value::Set(m) if m.class == cls::HASH_SET => Source::Keys(m.clone()),
        _ => Source::Items(items()?, Origin::of(recv)),
    };
    Ok(stream_value(StreamState::Pipeline { kind: Kind::Ref, source, stages: Vec::new(), used: false }))
}

/// `Pattern.splitAsStream(text)`.
pub fn split_stream(re: std::sync::Arc<crate::regex::Regex>, text: String) -> Value {
    stream_value(StreamState::Pipeline { kind: Kind::Ref, source: Source::Split(re, text), stages: Vec::new(), used: false })
}

/// A primitive stream (`IntStream` etc. by pipeline class) over the given elements.
pub fn prim_stream(class: ClassId, items: Vec<Value>) -> Value {
    stream_value(StreamState::Pipeline { kind: Kind::of_class(class), source: Source::Items(items, Origin::Unknown), stages: Vec::new(), used: false })
}

/// `CharSequence.chars()`.
pub fn int_stream(vm: &mut Vm, items: Vec<Value>) -> VR {
    let _ = vm;
    Ok(prim_stream(cls::INT_PIPELINE, items))
}

fn already_used() -> Thrown {
    Throw::new(cls::ILLEGAL_STATE_EXCEPTION, Some("stream has already been operated upon or closed".into()))
}

/// A terminal operation of a reference stream: reusing the stream fails in
/// `AbstractPipeline.evaluate`, called from the operation at `line` of ReferencePipeline.java;
/// failures inside the stream machinery get the JDK frames of the pipeline (see [`Run`]).
fn terminal(vm: &mut Vm, a: &mut [Value], t: TermInfo, f: fn(&mut Vm, &mut [Value]) -> VR) -> VR {
    let reference = a[0].class() == cls::REFERENCE_PIPELINE;
    let (_, head) = kind_of(&a[0]);
    traced(vm, a, t, f).map_err(|mut e| {
        if reference && t.for_each && head {
            let op = if t.method[0].contains("forEachOrdered") { "forEachOrdered" } else { "forEach" };
            return reuse_error(e, Kind::Ref, op, true);
        }
        if reference && e.class == cls::ILLEGAL_STATE_EXCEPTION && e.frames.is_empty() && e.offset.is_none() && e.message.as_deref() == Some("stream has already been operated upon or closed") {
            let check = if t.to_array { "java.util.stream.AbstractPipeline.evaluateToArrayNode(AbstractPipeline.java:277)" } else { "java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:260)" };
            let mut f = vec![check];
            f.extend(t.method);
            e.frames = jdk(&f);
        }
        e
    })
}

macro_rules! terminal_op {
    ($name:ident, $inner:ident, $t:expr) => {
        fn $name(vm: &mut Vm, a: &mut [Value]) -> VR {
            terminal(vm, a, $t, $inner)
        }
    };
}
terminal_op!(t_all_match, s_all_match, T_ALL_MATCH);
terminal_op!(t_any_match, s_any_match, T_ANY_MATCH);
terminal_op!(t_none_match, s_none_match, T_NONE_MATCH);
terminal_op!(t_collect, s_collect, T_COLLECT);
terminal_op!(t_collect3, s_collect3, T_COLLECT3);
terminal_op!(t_count, s_count, T_COUNT);
terminal_op!(t_find_any, s_find_first, T_FIND_ANY);
terminal_op!(t_find_first, s_find_first, T_FIND_FIRST);
terminal_op!(t_for_each, s_for_each, T_FOR_EACH);
terminal_op!(t_for_each_ordered, s_for_each, T_FOR_EACH_ORDERED);
terminal_op!(t_reduce1, s_reduce1, T_REDUCE1);
terminal_op!(t_reduce2, s_reduce2, T_REDUCE2);
terminal_op!(t_reduce3, s_reduce3, T_REDUCE3);
terminal_op!(t_max, s_max, T_MAX);
terminal_op!(t_min, s_min, T_MIN);
terminal_op!(t_to_array, s_to_array, T_TO_ARRAY);
terminal_op!(t_to_array_gen, s_to_array_gen, T_TO_ARRAY_GEN);

/// The JDK frames of a collector's own failure in `ReduceOps$3ReducingSink.accept` (a null
/// value or a duplicate key of `toMap`, a null `groupingBy` key, a non-`CharSequence` joined).
fn collector_site(mut e: Thrown, c: &CollectorDef) -> Thrown {
    if e.offset.is_some() || !e.frames.is_empty() {
        return e;
    }
    let msg = e.message.as_deref().unwrap_or("");
    let site: &[&str] = match c {
        CollectorDef::ToMap { merge: None, .. } if e.class == cls::NULL_POINTER_EXCEPTION && msg.is_empty() => {
            &["java.util.Objects.requireNonNull(Objects.java:220)", "java.util.stream.Collectors.lambda$uniqKeysMapAccumulator$0(Collectors.java:182)"]
        }
        CollectorDef::ToMap { merge: None, .. } if e.class == cls::ILLEGAL_STATE_EXCEPTION && msg.starts_with("Duplicate key") => {
            &["java.util.stream.Collectors.duplicateKeyException(Collectors.java:137)", "java.util.stream.Collectors.lambda$uniqKeysMapAccumulator$0(Collectors.java:184)"]
        }
        CollectorDef::GroupingBy { .. } if e.class == cls::NULL_POINTER_EXCEPTION && msg == "element cannot be mapped to a null key" => {
            &["java.util.Objects.requireNonNull(Objects.java:246)", "java.util.stream.Collectors.lambda$groupingBy$0(Collectors.java:1113)"]
        }
        CollectorDef::Joining { .. } if e.class == cls::CLASS_CAST_EXCEPTION => &[],
        _ => return e,
    };
    let mut f = site.to_vec();
    f.push("java.util.stream.ReduceOps$3ReducingSink.accept(ReduceOps.java:169)");
    e.frames = jdk(&f);
    e
}

/// The NPE of `Optional.of(null)` when a terminal operation's result is null, with the frames
/// of the operation's sink (`get`) and evaluation.
fn null_result(site: &[&str], t: TermInfo) -> Thrown {
    let mut f: Vec<&str> = vec!["java.util.Objects.requireNonNull(Objects.java:220)", "java.util.Optional.of(Optional.java:117)"];
    f.extend(site);
    f.extend(t.eval);
    f.extend(t.method);
    let mut e = Throw::new(cls::NULL_POINTER_EXCEPTION, None);
    e.frames = jdk(&f);
    e
}

fn state_of(v: &Value) -> Result<&RefCell<StreamState>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Stream(s) => Ok(s),
            _ => Err(crate::vm::cce("not a stream")),
        },
        _ => Err(npe("Cannot invoke a stream method because the stream is null")),
    }
}

/// Moves a pipeline out of a stream value (marking it used).
fn take(v: &Value) -> Result<(Kind, Source, Vec<Stage>), Thrown> {
    let mut s = state_of(v)?.borrow_mut();
    match &mut *s {
        StreamState::Pipeline { kind, source, stages, used } => {
            if *used {
                return Err(already_used());
            }
            *used = true;
            let src = std::mem::replace(source, Source::Consumed { head: stages.is_empty() });
            Ok((*kind, src, std::mem::take(stages)))
        }
        StreamState::Builder { .. } => Err(crate::vm::cce("not a stream")),
    }
}

fn with_stage(v: &Value, stage: Stage, new_kind: Option<Kind>, op: &str) -> VR {
    let (recv_kind, head) = kind_of(v);
    let (kind, source, mut stages) = take(v).map_err(|e| reuse_error(e, recv_kind, op, head))?;
    stages.push(stage);
    Ok(stream_value(StreamState::Pipeline { kind: new_kind.unwrap_or(kind), source, stages, used: false }))
}

// ------------------------------------------------------------------------------ evaluation

type Sink<'s> = dyn FnMut(&mut Vm, Value) -> Result<bool, Thrown> + 's;

/// The terminal operation a traced run evaluates, for the JDK frames of a failure inside the
/// stream machinery (outermost part: the evaluation and the operation's own methods).
#[derive(Clone, Copy)]
pub struct TermInfo {
    /// Frames between `wrapAndCopyInto` and the operation, innermost first.
    eval: &'static [&'static str],
    /// The operation's own frames (`ReferencePipeline.max`, …), innermost first.
    method: &'static [&'static str],
    /// The operation short-circuits (`findFirst`, `anyMatch`): the source is read with
    /// `tryAdvance` through `copyIntoWithCancel`.
    short: bool,
    /// `forEach`: a pipeline without stages runs `Head.forEach` on the source directly.
    for_each: bool,
    /// `toArray`: reusing the stream fails in `evaluateToArrayNode`.
    to_array: bool,
}

const EVAL_REDUCE: &[&str] = &["java.util.stream.ReduceOps$ReduceOp.evaluateSequential(ReduceOps.java:921)", "java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:265)"];
const EVAL_COUNT: &[&str] = &[
    "java.util.stream.ReduceOps$ReduceOp.evaluateSequential(ReduceOps.java:921)",
    "java.util.stream.ReduceOps$5.evaluateSequential(ReduceOps.java:258)",
    "java.util.stream.ReduceOps$5.evaluateSequential(ReduceOps.java:248)",
    "java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:265)",
];
const EVAL_FOR_EACH: &[&str] = &[
    "java.util.stream.ForEachOps$ForEachOp.evaluateSequential(ForEachOps.java:153)",
    "java.util.stream.ForEachOps$ForEachOp$OfRef.evaluateSequential(ForEachOps.java:176)",
    "java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:265)",
];
const EVAL_MATCH: &[&str] = &[
    "java.util.stream.MatchOps$MatchOp.evaluateSequential(MatchOps.java:230)",
    "java.util.stream.MatchOps$MatchOp.evaluateSequential(MatchOps.java:196)",
    "java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:265)",
];
const EVAL_FIND: &[&str] = &["java.util.stream.FindOps$FindOp.evaluateSequential(FindOps.java:150)", "java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:265)"];
const EVAL_TO_ARRAY: &[&str] = &["java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:635)", "java.util.stream.AbstractPipeline.evaluateToArrayNode(AbstractPipeline.java:291)"];

const fn term(eval: &'static [&'static str], method: &'static [&'static str], short: bool) -> TermInfo {
    TermInfo { eval, method, short, for_each: false, to_array: false }
}
const T_COLLECT: TermInfo = term(EVAL_REDUCE, &["java.util.stream.ReferencePipeline.collect(ReferencePipeline.java:723)"], false);
const T_COLLECT3: TermInfo = term(EVAL_REDUCE, &["java.util.stream.ReferencePipeline.collect(ReferencePipeline.java:734)"], false);
const T_REDUCE1: TermInfo = term(EVAL_REDUCE, &["java.util.stream.ReferencePipeline.reduce(ReferencePipeline.java:698)"], false);
const T_REDUCE2: TermInfo = term(EVAL_REDUCE, &["java.util.stream.ReferencePipeline.reduce(ReferencePipeline.java:693)"], false);
const T_REDUCE3: TermInfo = term(EVAL_REDUCE, &["java.util.stream.ReferencePipeline.reduce(ReferencePipeline.java:703)"], false);
const T_MAX: TermInfo = term(EVAL_REDUCE, &["java.util.stream.ReferencePipeline.reduce(ReferencePipeline.java:698)", "java.util.stream.ReferencePipeline.max(ReferencePipeline.java:739)"], false);
const T_MIN: TermInfo = term(EVAL_REDUCE, &["java.util.stream.ReferencePipeline.reduce(ReferencePipeline.java:698)", "java.util.stream.ReferencePipeline.min(ReferencePipeline.java:744)"], false);
const T_COUNT: TermInfo = term(EVAL_COUNT, &["java.util.stream.ReferencePipeline.count(ReferencePipeline.java:750)"], false);
const T_FOR_EACH: TermInfo = TermInfo { for_each: true, ..term(EVAL_FOR_EACH, &["java.util.stream.ReferencePipeline.forEach(ReferencePipeline.java:632)"], false) };
const T_FOR_EACH_ORDERED: TermInfo = TermInfo { for_each: true, ..term(EVAL_FOR_EACH, &["java.util.stream.ReferencePipeline.forEachOrdered(ReferencePipeline.java:637)"], false) };
const T_ANY_MATCH: TermInfo = term(EVAL_MATCH, &["java.util.stream.ReferencePipeline.anyMatch(ReferencePipeline.java:668)"], true);
const T_ALL_MATCH: TermInfo = term(EVAL_MATCH, &["java.util.stream.ReferencePipeline.allMatch(ReferencePipeline.java:673)"], true);
const T_NONE_MATCH: TermInfo = term(EVAL_MATCH, &["java.util.stream.ReferencePipeline.noneMatch(ReferencePipeline.java:678)"], true);
const T_FIND_FIRST: TermInfo = term(EVAL_FIND, &["java.util.stream.ReferencePipeline.findFirst(ReferencePipeline.java:683)"], true);
const T_FIND_ANY: TermInfo = term(EVAL_FIND, &["java.util.stream.ReferencePipeline.findAny(ReferencePipeline.java:688)"], true);
const T_TO_ARRAY_GEN: TermInfo = TermInfo { to_array: true, ..term(EVAL_TO_ARRAY, &["java.util.stream.ReferencePipeline.toArray(ReferencePipeline.java:652)"], false) };
const T_TO_ARRAY: TermInfo = TermInfo {
    to_array: true,
    ..term(EVAL_TO_ARRAY, &["java.util.stream.ReferencePipeline.toArray(ReferencePipeline.java:652)", "java.util.stream.ReferencePipeline.toArray(ReferencePipeline.java:658)"], false)
};

thread_local! {
    /// The terminal operation the next [`run`] evaluates for (set by [`traced`]; taken by the
    /// first run, so pipelines run inside it — `flatMap`, lambdas — are not traced).
    static TERMINAL: std::cell::Cell<Option<TermInfo>> = const { std::cell::Cell::new(None) };
}

/// Runs a reference stream's terminal operation with JDK frame tracing.
fn traced(vm: &mut Vm, a: &mut [Value], t: TermInfo, f: fn(&mut Vm, &mut [Value]) -> VR) -> VR {
    let reference = matches!(&a[0], Value::Obj(o) if matches!(&**o, Obj::Stream(s) if s.borrow().class() == cls::REFERENCE_PIPELINE));
    if !reference {
        return f(vm, a);
    }
    TERMINAL.with(|c| c.set(Some(t)));
    let r = f(vm, a);
    TERMINAL.with(|c| c.set(None));
    r
}

/// Accept frames of a stage's sink (`ReferencePipeline$3$1.accept` for `map`, …).
fn accept_frame(stage: &Stage) -> Option<&'static str> {
    Some(match stage {
        Stage::Filter(_) => "java.util.stream.ReferencePipeline$2$1.accept(ReferencePipeline.java:197)",
        Stage::Map(_, Kind::Ref) => "java.util.stream.ReferencePipeline$3$1.accept(ReferencePipeline.java:214)",
        Stage::Peek(_) => "java.util.stream.ReferencePipeline$15$1.accept(ReferencePipeline.java:577)",
        Stage::Distinct => "java.util.stream.DistinctOps$1$2.accept(DistinctOps.java:174)",
        Stage::Limit(_) | Stage::Skip(_) => "java.util.stream.SliceOps$1$1.accept(SliceOps.java:200)",
        _ => return None,
    })
}

/// How a run reaches its terminal sink, for failure frames.
struct Trace {
    term: TermInfo,
    origin: Origin,
    /// The source and every stage up to a barrier keep the size known (`SizedRefSortingSink`).
    sized: bool,
}

struct Run {
    stages: Vec<Stage>,
    /// Per-stage state: counters for limit/skip, buffers for sorted, seen sets for distinct.
    counters: Vec<i64>,
    buffers: Vec<Vec<Value>>,
    seen: Vec<HashSet<Key>>,
    /// Stage index at which the pipeline stopped accepting input.
    stopped: bool,
    trace: Option<Trace>,
    /// The terminal sink itself failed (not a script lambda of a stage): the failure gets the
    /// frames of the path its element took.
    sink_failed: bool,
}

const COPY_INTO: &str = "java.util.stream.AbstractPipeline.copyInto(AbstractPipeline.java:570)";
const COPY_INTO_END: &str = "java.util.stream.AbstractPipeline.copyInto(AbstractPipeline.java:571)";
const COPY_CANCEL: [&str; 3] = [
    "java.util.stream.ReferencePipeline.forEachWithCancel(ReferencePipeline.java:147)",
    "java.util.stream.AbstractPipeline.copyIntoWithCancel(AbstractPipeline.java:588)",
    "java.util.stream.AbstractPipeline.copyInto(AbstractPipeline.java:574)",
];
const COPY_CANCEL_END: [&str; 2] = ["java.util.stream.AbstractPipeline.copyIntoWithCancel(AbstractPipeline.java:589)", "java.util.stream.AbstractPipeline.copyInto(AbstractPipeline.java:574)"];
const WRAP_AND_COPY: &str = "java.util.stream.AbstractPipeline.wrapAndCopyInto(AbstractPipeline.java:560)";
const CHAINED_END: &str = "java.util.stream.Sink$ChainedReference.end(Sink.java:261)";

impl Run {
    fn new(stages: Vec<Stage>) -> Run {
        let n = stages.len();
        Run {
            stages,
            counters: vec![0; n],
            buffers: (0..n).map(|_| Vec::new()).collect(),
            seen: (0..n).map(|_| HashSet::new()).collect(),
            stopped: false,
            trace: None,
            sink_failed: false,
        }
    }

    /// A run that records the frames of failures for the terminal operation `term`.
    fn traced(stages: Vec<Stage>, src: &Source, term: TermInfo) -> Run {
        let origin = match src {
            Source::Items(_, o) => *o,
            Source::List(_) => Origin::ArrayList,
            Source::Keys(_) => Origin::HashKeys,
            _ => Origin::Unknown,
        };
        let sized = matches!(src, Source::Items(..) | Source::List(_) | Source::Keys(_));
        let mut r = Run::new(stages);
        r.trace = Some(Trace { term, origin, sized });
        r
    }

    /// The pipeline reads its source with `tryAdvance` (a short-circuiting terminal, or a
    /// `limit` stage).
    fn short(&self) -> bool {
        self.trace.as_ref().is_some_and(|t| t.term.short) || self.stages.iter().any(|s| matches!(s, Stage::Limit(_)))
    }

    /// The frames from a terminal-sink failure's element source up to the operation: the
    /// stages after `from` (a barrier's index, or the source), how that element got there, the
    /// evaluation. `None` when some part is not modelled.
    fn path_frames(&self, from: Option<usize>) -> Option<Vec<String>> {
        let t = self.trace.as_ref()?;
        let first = from.map_or(0, |b| b + 1);
        let mut f: Vec<&'static str> = Vec::new();
        for st in self.stages[first..].iter().rev() {
            f.push(accept_frame(st)?);
        }
        let short = self.short();
        match from {
            None => {
                if t.term.for_each && self.stages.is_empty() {
                    f.extend(t.origin.each()?);
                    f.push("java.util.stream.ReferencePipeline$Head.forEach(ReferencePipeline.java:803)");
                    return Some(jdk(&f));
                }
                if short {
                    f.push(t.origin.advance()?);
                    f.extend(COPY_CANCEL);
                } else {
                    f.extend(t.origin.each()?);
                    f.push(COPY_INTO);
                }
            }
            Some(b) => {
                if !self.sized_at(b) {
                    return None;
                }
                f.push("java.util.stream.SortedOps$SizedRefSortingSink.end(SortedOps.java:357)");
                self.end_chain(b, short, &mut f)?;
            }
        }
        f.push(WRAP_AND_COPY);
        f.extend(t.term.eval);
        f.extend(t.term.method);
        Some(jdk(&f))
    }

    /// The upstream of the barrier at `b` keeps the size known.
    fn sized_at(&self, b: usize) -> bool {
        self.trace.as_ref().is_some_and(|t| t.sized)
            && self.stages[..b].iter().all(|s| matches!(s, Stage::Map(..) | Stage::Peek(_) | Stage::Limit(_) | Stage::Skip(_)))
    }

    /// The `end()` calls from `copyInto` down to the barrier at `b`.
    fn end_chain(&self, b: usize, short: bool, f: &mut Vec<&'static str>) -> Option<()> {
        for st in self.stages[..b].iter().rev() {
            if !matches!(st, Stage::Map(_, Kind::Ref) | Stage::Filter(_) | Stage::Peek(_)) {
                return None;
            }
            f.push(CHAINED_END);
        }
        if short {
            f.extend(COPY_CANCEL_END);
        } else {
            f.push(COPY_INTO_END);
        }
        Some(())
    }

    /// The frames above a failed comparison of the sort barrier at `b`.
    fn sort_frames(&self, b: usize) -> Option<Vec<String>> {
        let t = self.trace.as_ref()?;
        let mut f: Vec<&'static str> = vec!["java.util.Arrays.sort(Arrays.java:1304)"];
        if self.sized_at(b) {
            f.push("java.util.stream.SortedOps$SizedRefSortingSink.end(SortedOps.java:353)");
        } else {
            f.extend(["java.util.ArrayList.sortRange(ArrayList.java:1825)", "java.util.ArrayList.sort(ArrayList.java:1818)", "java.util.stream.SortedOps$RefSortingSink.end(SortedOps.java:392)"]);
        }
        self.end_chain(b, self.short(), &mut f)?;
        f.push(WRAP_AND_COPY);
        f.extend(t.term.eval);
        f.extend(t.term.method);
        Some(jdk(&f))
    }

    /// A failure of the terminal sink or the source: appends the path's frames.
    fn finish_error(&mut self, mut e: Thrown, from: Option<usize>) -> Thrown {
        if std::mem::take(&mut self.sink_failed)
            && let Some(f) = self.path_frames(from)
        {
            e.frames.extend(f);
        }
        e
    }

    /// Pushes `v` into stage `k`; `Ok(false)` means downstream wants no more elements.
    fn push(&mut self, vm: &mut Vm, k: usize, v: Value, sink: &mut Sink) -> Result<bool, Thrown> {
        if k == self.stages.len() {
            let r = sink(vm, v);
            if let Err(e) = &r
                && e.offset.is_none()
            {
                self.sink_failed = true;
            }
            return r;
        }
        match &self.stages[k] {
            Stage::Filter(f) => {
                let f = f.clone();
                if call_bool(vm, &f, std::slice::from_ref(&v))? {
                    self.push(vm, k + 1, v, sink)
                } else {
                    Ok(true)
                }
            }
            Stage::Map(f, kind) => {
                let (f, kind) = (f.clone(), *kind);
                let r = call(vm, &f, &[v])?;
                let r = map_result(r, kind)?;
                self.push(vm, k + 1, r, sink)
            }
            Stage::Convert(kind) => {
                let kind = *kind;
                self.push(vm, k + 1, kind.conv(v), sink)
            }
            Stage::FlatMap(f) => {
                let f = f.clone();
                let r = call(vm, &f, &[v])?;
                if r.is_null() {
                    return Ok(true);
                }
                let (_, src, stages) = take(&r)?;
                let mut inner = Run::new(stages);
                let mut cont = true;
                inner.drive(vm, src, &mut |vm, x| {
                    let c = self.push(vm, k + 1, x, sink)?;
                    cont = c;
                    Ok(c)
                })?;
                // the inner pipeline's frames are not modelled
                self.sink_failed = false;
                Ok(cont)
            }
            Stage::Peek(f) => {
                let f = f.clone();
                call(vm, &f, std::slice::from_ref(&v))?;
                self.push(vm, k + 1, v, sink)
            }
            Stage::Distinct => {
                if self.seen[k].insert(Key(v.clone())) {
                    self.push(vm, k + 1, v, sink)
                } else {
                    Ok(true)
                }
            }
            Stage::Sorted(_) => {
                self.buffers[k].push(v);
                Ok(true)
            }
            Stage::Limit(n) => {
                let n = *n;
                if self.counters[k] >= n {
                    return Ok(false);
                }
                self.counters[k] += 1;
                let c = self.push(vm, k + 1, v, sink)?;
                Ok(c && self.counters[k] < n)
            }
            Stage::Skip(n) => {
                if self.counters[k] < *n {
                    self.counters[k] += 1;
                    return Ok(true);
                }
                self.push(vm, k + 1, v, sink)
            }
        }
    }

    /// End of input: flushes barriers in pipeline order.
    fn end(&mut self, vm: &mut Vm, from: usize, sink: &mut Sink) -> Result<(), Thrown> {
        for k in from..self.stages.len() {
            if let Stage::Sorted(cmp) = &self.stages[k] {
                let cmp = cmp.clone();
                let mut items = std::mem::take(&mut self.buffers[k]);
                // a stream sorts with `Comparator.naturalOrder()` when given no comparator
                let sorted = match &cmp {
                    None => crate::sort::timsort(&mut items, crate::sort::Flavor::Comparator, &mut |a, b| func::natural_order_compare(a, b)),
                    Some(c) => func::sort_values(vm, &mut items, c),
                };
                if let Err(mut e) = sorted {
                    if e.offset.is_none()
                        && let Some(f) = self.sort_frames(k)
                    {
                        e.frames.extend(f);
                    }
                    return Err(e);
                }
                for it in items {
                    match self.push(vm, k + 1, it, sink) {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(e) => return Err(self.finish_error(e, Some(k))),
                    }
                }
            }
        }
        Ok(())
    }

    fn drive(&mut self, vm: &mut Vm, src: Source, sink: &mut Sink) -> Result<(), Thrown> {
        match src {
            Source::Items(items, _) | Source::Unsized(items) => {
                for it in items {
                    match self.push(vm, 0, it, sink) {
                        Ok(true) => {}
                        Ok(false) => {
                            self.stopped = true;
                            break;
                        }
                        Err(e) => return Err(self.finish_error(e, None)),
                    }
                }
            }
            Source::List(l) => {
                // late binding: the size and modCount the traversal expects are read now; the
                // elements are read as it goes (writes to later ones are seen)
                l.sync_view();
                let n = l.len();
                let expected = l.mods.get();
                for i in 0..n {
                    let it = l.v.borrow().get(i).cloned().unwrap_or(Value::Null);
                    match self.push(vm, 0, it, sink) {
                        Ok(true) => {}
                        Ok(false) => {
                            self.stopped = true;
                            break;
                        }
                        Err(e) => return Err(self.finish_error(e, None)),
                    }
                    if self.short() && l.mods.get() != expected {
                        break;
                    }
                }
                if l.mods.get() != expected {
                    let mut e = Throw::new(cls::CONCURRENT_MODIFICATION_EXCEPTION, None);
                    let short = self.short();
                    if let Some(mut f) = self.path_frames(None) {
                        // the spliterator checks after the traversal (or after each element)
                        let check = if short { "java.base/java.util.ArrayList$ArrayListSpliterator.tryAdvance(ArrayList.java:1703)" } else { "java.base/java.util.ArrayList$ArrayListSpliterator.forEachRemaining(ArrayList.java:1730)" };
                        let skip = self.stages.iter().filter(|s| accept_frame(s).is_some()).count() + 1;
                        f.drain(..skip.min(f.len()));
                        f.insert(0, check.to_string());
                        e.frames = f;
                    }
                    return Err(e);
                }
            }
            Source::Consumed { .. } => {}
            Source::Keys(m) => {
                let (items, expected) = (m.keys(), m.mods.get());
                for it in items {
                    match self.push(vm, 0, it, sink) {
                        Ok(true) => {}
                        Ok(false) => {
                            self.stopped = true;
                            break;
                        }
                        Err(e) => return Err(self.finish_error(e, None)),
                    }
                }
                if m.mods.get() != expected {
                    let mut e = Throw::new(cls::CONCURRENT_MODIFICATION_EXCEPTION, None);
                    if !self.short()
                        && let Some(mut f) = self.path_frames(None)
                    {
                        let skip = self.stages.iter().filter(|s| accept_frame(s).is_some()).count() + 1;
                        f.drain(..skip.min(f.len()));
                        f.insert(0, "java.base/java.util.HashMap$KeySpliterator.forEachRemaining(HashMap.java:1746)".to_string());
                        e.frames = f;
                    }
                    return Err(e);
                }
            }
            Source::Split(re, text) => {
                let items = match crate::regex::split_stream_parts(vm, re, &text) {
                    Ok(items) => items,
                    Err(mut e) => {
                        // the regex limit trips in `MatcherIterator.hasNext`
                        if e.offset.is_none()
                            && let Some(t) = &self.trace
                            && !self.short()
                        {
                            let mut f = vec![
                                "java.util.regex.Pattern$1MatcherIterator.hasNext(Pattern.java:6117)",
                                "java.util.Iterator.forEachRemaining(Iterator.java:132)",
                                "java.util.Spliterators$IteratorSpliterator.forEachRemaining(Spliterators.java:1939)",
                                COPY_INTO,
                                WRAP_AND_COPY,
                            ];
                            f.extend(t.term.eval);
                            f.extend(t.term.method);
                            e.frames.extend(jdk(&f));
                        }
                        return Err(e);
                    }
                };
                for it in items {
                    match self.push(vm, 0, it, sink) {
                        Ok(true) => {}
                        Ok(false) => {
                            self.stopped = true;
                            break;
                        }
                        Err(e) => return Err(self.finish_error(e, None)),
                    }
                }
            }
            Source::Concat(a, b) => {
                for part in [*a, *b] {
                    let StreamState::Pipeline { source, stages, .. } = part else { continue };
                    let mut inner = Run::new(stages);
                    let mut cont = true;
                    inner.drive(vm, source, &mut |vm, x| {
                        let c = self.push(vm, 0, x, sink)?;
                        cont = c;
                        Ok(c)
                    })?;
                    self.sink_failed = false;
                    if !cont {
                        self.stopped = true;
                        break;
                    }
                }
            }
        }
        self.end(vm, 0, sink)
    }
}

/// JDK frames as Elasticsearch prints them (`java.base/` module prefix).
fn jdk(frames: &[&str]) -> Vec<String> {
    frames.iter().map(|f| format!("java.base/{f}")).collect()
}

fn map_result(r: Value, kind: Kind) -> VR {
    Ok(match kind {
        Kind::Ref => r,
        Kind::Int => Value::Int(crate::cast::as_int(&crate::cast::from_return(r, Ty::INT)?)?),
        Kind::Long => Value::Long(crate::cast::as_long(&crate::cast::from_return(r, Ty::LONG)?)?),
        Kind::Double => Value::Double(crate::cast::as_double(&crate::cast::from_return(r, Ty::DOUBLE)?)?),
    })
}

/// Runs a stream to completion, feeding `sink`; returns the stream's element kind.
fn run(vm: &mut Vm, v: &Value, sink: &mut Sink) -> Result<Kind, Thrown> {
    let term = TERMINAL.with(|c| c.take());
    let (kind, src, stages) = take(v)?;
    let mut r = match term {
        Some(t) => Run::traced(stages, &src, t),
        None => Run::new(stages),
    };
    r.drive(vm, src, sink)?;
    Ok(kind)
}

/// Runs a stream to completion and returns its elements (`spliterator()`).
pub fn stream_items(vm: &mut Vm, v: &Value) -> Result<Vec<Value>, Thrown> {
    Ok(collect_all(vm, v)?.1)
}

fn collect_all(vm: &mut Vm, v: &Value) -> Result<(Kind, Vec<Value>), Thrown> {
    let mut out = Vec::new();
    let kind = run(vm, v, &mut |_, x| {
        out.push(x);
        Ok(true)
    })?;
    Ok((kind, out))
}

// ------------------------------------------------------------------------------ reuse frames

/// The JDK frames of reusing a consumed stream in operation `op` of a `kind` pipeline:
/// intermediate operations fail constructing the next stage (`AbstractPipeline.<init>`),
/// terminal ones in `evaluate` (`evaluateToArrayNode`, `spliterator`). `None` when not modelled.
fn reuse_frames(kind: Kind, op: &str, head: bool) -> Option<Vec<String>> {
    const SL: bool = false;
    const SF: bool = true;
    // intermediate: (pipeline kind constructed, stateful, op frames innermost first)
    let inter: Option<(Kind, bool, &[&str])> = match (kind, op) {
        (Kind::Ref, "filter") => Some((Kind::Ref, SL, &["ReferencePipeline$2.<init>(ReferencePipeline.java:185)", "ReferencePipeline.filter(ReferencePipeline.java:184)"])),
        (Kind::Ref, "map") => Some((Kind::Ref, SL, &["ReferencePipeline$3.<init>(ReferencePipeline.java:208)", "ReferencePipeline.map(ReferencePipeline.java:207)"])),
        (Kind::Ref, "peek") => Some((Kind::Ref, SL, &["ReferencePipeline$15.<init>(ReferencePipeline.java:570)", "ReferencePipeline.peek(ReferencePipeline.java:569)"])),
        (Kind::Ref, "skip") => Some((Kind::Ref, SF, &["SliceOps$1.<init>(SliceOps.java:111)", "SliceOps.makeRef(SliceOps.java:111)", "ReferencePipeline.skip(ReferencePipeline.java:615)"])),
        (Kind::Ref, "limit") => Some((Kind::Ref, SF, &["SliceOps$1.<init>(SliceOps.java:111)", "SliceOps.makeRef(SliceOps.java:111)", "ReferencePipeline.limit(ReferencePipeline.java:605)"])),
        (Kind::Ref, "flatMap") => Some((Kind::Ref, SL, &["ReferencePipeline$7.<init>(ReferencePipeline.java:276)", "ReferencePipeline.flatMap(ReferencePipeline.java:275)"])),
        (Kind::Ref, "mapToInt") => Some((Kind::Int, SL, &["ReferencePipeline$4.<init>(ReferencePipeline.java:225)", "ReferencePipeline.mapToInt(ReferencePipeline.java:224)"])),
        (Kind::Ref, "mapToLong") => Some((Kind::Long, SL, &["ReferencePipeline$5.<init>(ReferencePipeline.java:242)", "ReferencePipeline.mapToLong(ReferencePipeline.java:241)"])),
        (Kind::Ref, "mapToDouble") => Some((Kind::Double, SL, &["ReferencePipeline$6.<init>(ReferencePipeline.java:259)", "ReferencePipeline.mapToDouble(ReferencePipeline.java:258)"])),
        (Kind::Ref, "flatMapToInt") => Some((Kind::Int, SL, &["ReferencePipeline$8.<init>(ReferencePipeline.java:322)", "ReferencePipeline.flatMapToInt(ReferencePipeline.java:321)"])),
        (Kind::Ref, "flatMapToLong") => Some((Kind::Long, SL, &["ReferencePipeline$10.<init>(ReferencePipeline.java:425)", "ReferencePipeline.flatMapToLong(ReferencePipeline.java:424)"])),
        (Kind::Ref, "flatMapToDouble") => Some((Kind::Double, SL, &["ReferencePipeline$9.<init>(ReferencePipeline.java:373)", "ReferencePipeline.flatMapToDouble(ReferencePipeline.java:372)"])),
        (Kind::Ref, "sorted") => Some((
            Kind::Ref,
            SF,
            &["SortedOps$OfRef.<init>(SortedOps.java:127)", "SortedOps$OfRef.<init>(SortedOps.java:114)", "SortedOps.makeRef(SortedOps.java:51)", "ReferencePipeline.sorted(ReferencePipeline.java:593)"],
        )),
        (Kind::Ref, "sortedCmp") => Some((Kind::Ref, SF, &["SortedOps$OfRef.<init>(SortedOps.java:127)", "SortedOps.makeRef(SortedOps.java:63)", "ReferencePipeline.sorted(ReferencePipeline.java:598)"])),
        (Kind::Ref, "distinct") => Some((Kind::Ref, SF, &["DistinctOps$1.<init>(DistinctOps.java:56)", "DistinctOps.makeRef(DistinctOps.java:55)", "ReferencePipeline.distinct(ReferencePipeline.java:588)"])),
        (Kind::Int, "filter") => Some((Kind::Int, SL, &["IntPipeline$10.<init>(IntPipeline.java:383)", "IntPipeline.filter(IntPipeline.java:382)"])),
        (Kind::Int, "map") => Some((Kind::Int, SL, &["IntPipeline$4.<init>(IntPipeline.java:240)", "IntPipeline.map(IntPipeline.java:239)"])),
        (Kind::Int, "peek") => Some((Kind::Int, SL, &["IntPipeline$11.<init>(IntPipeline.java:406)", "IntPipeline.peek(IntPipeline.java:405)"])),
        (Kind::Int, "skip") => Some((Kind::Int, SF, &["SliceOps$2.<init>(SliceOps.java:233)", "SliceOps.makeInt(SliceOps.java:233)", "IntPipeline.skip(IntPipeline.java:436)"])),
        (Kind::Int, "limit") => Some((Kind::Int, SF, &["SliceOps$2.<init>(SliceOps.java:233)", "SliceOps.makeInt(SliceOps.java:233)", "IntPipeline.limit(IntPipeline.java:426)"])),
        (Kind::Int, "sorted") => Some((Kind::Int, SF, &["SortedOps$OfInt.<init>(SortedOps.java:169)", "SortedOps.makeInt(SortedOps.java:73)", "IntPipeline.sorted(IntPipeline.java:451)"])),
        (Kind::Int, "distinct") => Some((
            Kind::Ref,
            SL,
            &["IntPipeline$1.<init>(IntPipeline.java:174)", "IntPipeline.mapToObj(IntPipeline.java:174)", "IntPipeline.boxed(IntPipeline.java:233)", "IntPipeline.distinct(IntPipeline.java:458)"],
        )),
        (Kind::Int, "boxed") => Some((Kind::Ref, SL, &["IntPipeline$1.<init>(IntPipeline.java:174)", "IntPipeline.mapToObj(IntPipeline.java:174)", "IntPipeline.boxed(IntPipeline.java:233)"])),
        (Kind::Int, "mapToObj") => Some((Kind::Ref, SL, &["IntPipeline$1.<init>(IntPipeline.java:174)", "IntPipeline.mapToObj(IntPipeline.java:174)", "IntPipeline.mapToObj(IntPipeline.java:256)"])),
        (Kind::Int, "asLongStream") => Some((Kind::Long, SL, &["IntPipeline$2.<init>(IntPipeline.java:203)", "IntPipeline.asLongStream(IntPipeline.java:203)"])),
        (Kind::Int, "asDoubleStream") => Some((Kind::Double, SL, &["IntPipeline$3.<init>(IntPipeline.java:218)", "IntPipeline.asDoubleStream(IntPipeline.java:218)"])),
        (Kind::Int, "mapToLong") => Some((Kind::Long, SL, &["IntPipeline$5.<init>(IntPipeline.java:263)", "IntPipeline.mapToLong(IntPipeline.java:262)"])),
        (Kind::Int, "mapToDouble") => Some((Kind::Double, SL, &["IntPipeline$6.<init>(IntPipeline.java:280)", "IntPipeline.mapToDouble(IntPipeline.java:279)"])),
        (Kind::Int, "flatMap") => Some((Kind::Int, SL, &["IntPipeline$7.<init>(IntPipeline.java:297)", "IntPipeline.flatMap(IntPipeline.java:296)"])),
        (Kind::Long, "filter") => Some((Kind::Long, SL, &["LongPipeline$9.<init>(LongPipeline.java:366)", "LongPipeline.filter(LongPipeline.java:365)"])),
        (Kind::Long, "map") => Some((Kind::Long, SL, &["LongPipeline$3.<init>(LongPipeline.java:223)", "LongPipeline.map(LongPipeline.java:222)"])),
        (Kind::Long, "peek") => Some((Kind::Long, SL, &["LongPipeline$10.<init>(LongPipeline.java:389)", "LongPipeline.peek(LongPipeline.java:388)"])),
        (Kind::Long, "skip") => Some((Kind::Long, SF, &["SliceOps$3.<init>(SliceOps.java:348)", "SliceOps.makeLong(SliceOps.java:348)", "LongPipeline.skip(LongPipeline.java:419)"])),
        (Kind::Long, "limit") => Some((Kind::Long, SF, &["SliceOps$3.<init>(SliceOps.java:348)", "SliceOps.makeLong(SliceOps.java:348)", "LongPipeline.limit(LongPipeline.java:409)"])),
        (Kind::Long, "sorted") => Some((Kind::Long, SF, &["SortedOps$OfLong.<init>(SortedOps.java:208)", "SortedOps.makeLong(SortedOps.java:83)", "LongPipeline.sorted(LongPipeline.java:434)"])),
        (Kind::Long, "distinct") => Some((
            Kind::Ref,
            SL,
            &["LongPipeline$1.<init>(LongPipeline.java:172)", "LongPipeline.mapToObj(LongPipeline.java:172)", "LongPipeline.boxed(LongPipeline.java:216)", "LongPipeline.distinct(LongPipeline.java:441)"],
        )),
        (Kind::Long, "boxed") => Some((Kind::Ref, SL, &["LongPipeline$1.<init>(LongPipeline.java:172)", "LongPipeline.mapToObj(LongPipeline.java:172)", "LongPipeline.boxed(LongPipeline.java:216)"])),
        (Kind::Long, "mapToObj") => Some((Kind::Ref, SL, &["LongPipeline$1.<init>(LongPipeline.java:172)", "LongPipeline.mapToObj(LongPipeline.java:172)", "LongPipeline.mapToObj(LongPipeline.java:239)"])),
        (Kind::Long, "asDoubleStream") => Some((Kind::Double, SL, &["LongPipeline$2.<init>(LongPipeline.java:201)", "LongPipeline.asDoubleStream(LongPipeline.java:201)"])),
        (Kind::Long, "mapToDouble") => Some((Kind::Double, SL, &["LongPipeline$5.<init>(LongPipeline.java:263)", "LongPipeline.mapToDouble(LongPipeline.java:262)"])),
        (Kind::Long, "mapToInt") => Some((Kind::Int, SL, &["LongPipeline$4.<init>(LongPipeline.java:246)", "LongPipeline.mapToInt(LongPipeline.java:245)"])),
        (Kind::Long, "flatMap") => Some((Kind::Long, SL, &["LongPipeline$6.<init>(LongPipeline.java:280)", "LongPipeline.flatMap(LongPipeline.java:279)"])),
        (Kind::Double, "filter") => Some((Kind::Double, SL, &["DoublePipeline$8.<init>(DoublePipeline.java:351)", "DoublePipeline.filter(DoublePipeline.java:350)"])),
        (Kind::Double, "map") => Some((Kind::Double, SL, &["DoublePipeline$2.<init>(DoublePipeline.java:207)", "DoublePipeline.map(DoublePipeline.java:206)"])),
        (Kind::Double, "peek") => Some((Kind::Double, SL, &["DoublePipeline$9.<init>(DoublePipeline.java:374)", "DoublePipeline.peek(DoublePipeline.java:373)"])),
        (Kind::Double, "skip") => Some((Kind::Double, SF, &["SliceOps$4.<init>(SliceOps.java:463)", "SliceOps.makeDouble(SliceOps.java:463)", "DoublePipeline.skip(DoublePipeline.java:405)"])),
        (Kind::Double, "limit") => Some((Kind::Double, SF, &["SliceOps$4.<init>(SliceOps.java:463)", "SliceOps.makeDouble(SliceOps.java:463)", "DoublePipeline.limit(DoublePipeline.java:394)"])),
        (Kind::Double, "sorted") => Some((Kind::Double, SF, &["SortedOps$OfDouble.<init>(SortedOps.java:247)", "SortedOps.makeDouble(SortedOps.java:93)", "DoublePipeline.sorted(DoublePipeline.java:421)"])),
        (Kind::Double, "distinct") => Some((
            Kind::Ref,
            SL,
            &["DoublePipeline$1.<init>(DoublePipeline.java:171)", "DoublePipeline.mapToObj(DoublePipeline.java:171)", "DoublePipeline.boxed(DoublePipeline.java:200)", "DoublePipeline.distinct(DoublePipeline.java:428)"],
        )),
        (Kind::Double, "boxed") => Some((Kind::Ref, SL, &["DoublePipeline$1.<init>(DoublePipeline.java:171)", "DoublePipeline.mapToObj(DoublePipeline.java:171)", "DoublePipeline.boxed(DoublePipeline.java:200)"])),
        (Kind::Double, "mapToObj") => Some((Kind::Ref, SL, &["DoublePipeline$1.<init>(DoublePipeline.java:171)", "DoublePipeline.mapToObj(DoublePipeline.java:171)", "DoublePipeline.mapToObj(DoublePipeline.java:223)"])),
        (Kind::Double, "mapToLong") => Some((Kind::Long, SL, &["DoublePipeline$4.<init>(DoublePipeline.java:247)", "DoublePipeline.mapToLong(DoublePipeline.java:246)"])),
        (Kind::Double, "mapToInt") => Some((Kind::Int, SL, &["DoublePipeline$3.<init>(DoublePipeline.java:230)", "DoublePipeline.mapToInt(DoublePipeline.java:229)"])),
        (Kind::Double, "flatMap") => Some((Kind::Double, SL, &["DoublePipeline$5.<init>(DoublePipeline.java:264)", "DoublePipeline.flatMap(DoublePipeline.java:263)"])),
        _ => None,
    };
    let mut f: Vec<String> = Vec::new();
    if let Some((new_kind, stateful, tail)) = inter {
        let (pipe, init, sl, sf) = match new_kind {
            Kind::Ref => ("ReferencePipeline", 101, 841, 870),
            Kind::Int => ("IntPipeline", 91, 654, 681),
            Kind::Long => ("LongPipeline", 92, 636, 663),
            Kind::Double => ("DoublePipeline", 91, 665, 693),
        };
        f.push("AbstractPipeline.<init>(AbstractPipeline.java:201)".into());
        f.push(format!("{pipe}.<init>({pipe}.java:{init})"));
        if stateful {
            f.push(format!("{pipe}$StatefulOp.<init>({pipe}.java:{sf})"));
        } else {
            f.push(format!("{pipe}$StatelessOp.<init>({pipe}.java:{sl})"));
        }
        f.extend(tail.iter().map(|s| s.to_string()));
        return Some(f.into_iter().map(|s| format!("java.base/java.util.stream.{s}")).collect());
    }
    // terminal operations of primitive pipelines (reference ones carry a `TermInfo`)
    let (pipe, lines): (&str, [u32; 18]) = match kind {
        // sum, reduce2, max, reduce1, min, count, average, collect, stats, toArray, spliterator,
        // iterator, anyMatch, allMatch, noneMatch, findFirst, findAny, forEach (forEachOrdered = +5)
        Kind::Int => ("IntPipeline", [475, 517, 485, 522, 480, 490, 495, 534, 511, 564, 196, 191, 539, 544, 549, 554, 559, 465]),
        Kind::Long => ("LongPipeline", [459, 501, 469, 506, 464, 490, 474, 518, 495, 548, 194, 189, 523, 528, 533, 538, 543, 448]),
        Kind::Double => ("DoublePipeline", [453, 544, 475, 532, 470, 516, 496, 544, 521, 574, 193, 188, 549, 554, 559, 564, 569, 435]),
        Kind::Ref => {
            return match op {
                "iterator" => Some(jdk(&["java.util.stream.AbstractPipeline.spliterator(AbstractPipeline.java:378)", "java.util.stream.ReferencePipeline.iterator(ReferencePipeline.java:161)"])),
                "forEach" if head => Some(jdk(&["java.util.stream.AbstractPipeline.sourceStageSpliterator(AbstractPipeline.java:311)", "java.util.stream.ReferencePipeline$Head.forEach(ReferencePipeline.java:803)"])),
                "forEachOrdered" if head => Some(jdk(&["java.util.stream.AbstractPipeline.sourceStageSpliterator(AbstractPipeline.java:311)", "java.util.stream.ReferencePipeline$Head.forEachOrdered(ReferencePipeline.java:813)"])),
                _ => None,
            };
        }
    };
    let eval = "AbstractPipeline.evaluate(AbstractPipeline.java:260)".to_string();
    let m = |name: &str, line: u32| format!("{pipe}.{name}({pipe}.java:{line})");
    // `sum` of a DoublePipeline collects (compensated summation); the others reduce
    let sum_via = if kind == Kind::Double { "collect" } else { "reduce" };
    match op {
        "sum" => f.extend([eval, m(sum_via, lines[1]), m("sum", lines[0])]),
        "max" => f.extend([eval, m("reduce", lines[3]), m("max", lines[2])]),
        "min" => f.extend([eval, m("reduce", lines[3]), m("min", lines[4])]),
        "count" => f.extend([eval, m("count", lines[5])]),
        "average" => f.extend([eval, m("collect", lines[7]), m("average", lines[6])]),
        "summaryStatistics" => f.extend([eval, m("collect", lines[7]), m("summaryStatistics", lines[8])]),
        "collect" => f.extend([eval, m("collect", lines[7])]),
        "reduce2" => f.extend([eval, m("reduce", if kind == Kind::Double { 527 } else { lines[1] })]),
        "reduce1" => f.extend([eval, m("reduce", lines[3])]),
        "toArray" => f.extend(["AbstractPipeline.evaluateToArrayNode(AbstractPipeline.java:277)".to_string(), m("toArray", lines[9])]),
        "iterator" => f.extend(["AbstractPipeline.spliterator(AbstractPipeline.java:378)".to_string(), m("spliterator", lines[10]), m("iterator", lines[11])]),
        "anyMatch" => f.extend([eval, m("anyMatch", lines[12])]),
        "allMatch" => f.extend([eval, m("allMatch", lines[13])]),
        "noneMatch" => f.extend([eval, m("noneMatch", lines[14])]),
        "findFirst" => f.extend([eval, m("findFirst", lines[15])]),
        "findAny" => f.extend([eval, m("findAny", lines[16])]),
        "forEach" => f.extend([eval, m("forEach", lines[17])]),
        "forEachOrdered" => f.extend([eval, m("forEachOrdered", lines[17] + 5)]),
        _ => return None,
    }
    Some(f.into_iter().map(|s| format!("java.base/java.util.stream.{s}")).collect())
}

/// Gives a reuse failure of `op` its JDK frames (see [`reuse_frames`]).
fn reuse_error(mut e: Thrown, kind: Kind, op: &str, head: bool) -> Thrown {
    if e.class == cls::ILLEGAL_STATE_EXCEPTION && e.frames.is_empty() && e.offset.is_none() && e.message.as_deref() == Some("stream has already been operated upon or closed")
        && let Some(f) = reuse_frames(kind, op, head)
    {
        e.frames = f;
    }
    e
}

/// The kind of a stream value and whether it was consumed as a bare source (`Head`).
fn kind_of(v: &Value) -> (Kind, bool) {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Stream(s) => {
                let s = s.borrow();
                let head = matches!(&*s, StreamState::Pipeline { source: Source::Consumed { head: true }, .. });
                (Kind::of_class(s.class()), head)
            }
            _ => (Kind::Ref, false),
        },
        _ => (Kind::Ref, false),
    }
}

macro_rules! prim_terminal {
    ($name:ident, $inner:ident, $op:literal) => {
        fn $name(vm: &mut Vm, a: &mut [Value]) -> VR {
            let (kind, head) = kind_of(&a[0]);
            $inner(vm, a).map_err(|e| reuse_error(e, kind, $op, head))
        }
    };
}
prim_terminal!(p_sum, s_sum, "sum");
prim_terminal!(p_max, s_prim_max, "max");
prim_terminal!(p_min, s_prim_min, "min");
prim_terminal!(p_count, s_count, "count");
prim_terminal!(p_average, s_average, "average");
prim_terminal!(p_summary, s_summary, "summaryStatistics");
prim_terminal!(p_to_array, s_to_array, "toArray");
prim_terminal!(p_iterator, s_iterator, "iterator");
prim_terminal!(p_any_match, s_any_match, "anyMatch");
prim_terminal!(p_all_match, s_all_match, "allMatch");
prim_terminal!(p_none_match, s_none_match, "noneMatch");
prim_terminal!(p_find_first, s_find_first, "findFirst");
prim_terminal!(p_find_any, s_find_first, "findAny");
prim_terminal!(p_for_each, s_for_each, "forEach");
prim_terminal!(p_for_each_ordered, s_for_each, "forEachOrdered");
prim_terminal!(p_reduce1, s_reduce1, "reduce1");
prim_terminal!(p_reduce2, s_reduce2, "reduce2");
prim_terminal!(p_collect3, s_collect3, "collect");

// ------------------------------------------------------------------------------ intermediate ops

fn s_filter(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Filter(a[1].clone()), None, "filter")
}
fn s_map(_: &mut Vm, a: &mut [Value]) -> VR {
    let kind = Kind::of_class(state_of(&a[0])?.borrow().class());
    with_stage(&a[0], Stage::Map(a[1].clone(), kind), None, "map")
}
fn map_to(a: &mut [Value], k: Kind) -> VR {
    let op = match k {
        Kind::Ref => "mapToObj",
        Kind::Int => "mapToInt",
        Kind::Long => "mapToLong",
        Kind::Double => "mapToDouble",
    };
    with_stage(&a[0], Stage::Map(a[1].clone(), k), Some(k), op)
}
fn s_map_to_obj(_: &mut Vm, a: &mut [Value]) -> VR {
    map_to(a, Kind::Ref)
}
fn s_map_to_int(_: &mut Vm, a: &mut [Value]) -> VR {
    map_to(a, Kind::Int)
}
fn s_map_to_long(_: &mut Vm, a: &mut [Value]) -> VR {
    map_to(a, Kind::Long)
}
fn s_map_to_double(_: &mut Vm, a: &mut [Value]) -> VR {
    map_to(a, Kind::Double)
}
fn s_flat_map(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::FlatMap(a[1].clone()), None, "flatMap")
}
fn s_flat_map_to(a: &mut [Value], k: Kind) -> VR {
    let op = match k {
        Kind::Int => "flatMapToInt",
        Kind::Long => "flatMapToLong",
        _ => "flatMapToDouble",
    };
    with_stage(&a[0], Stage::FlatMap(a[1].clone()), Some(k), op)
}
fn s_flat_map_to_int(_: &mut Vm, a: &mut [Value]) -> VR {
    s_flat_map_to(a, Kind::Int)
}
fn s_flat_map_to_long(_: &mut Vm, a: &mut [Value]) -> VR {
    s_flat_map_to(a, Kind::Long)
}
fn s_flat_map_to_double(_: &mut Vm, a: &mut [Value]) -> VR {
    s_flat_map_to(a, Kind::Double)
}
fn s_peek(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Peek(a[1].clone()), None, "peek")
}
fn s_distinct(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Distinct, None, "distinct")
}
fn s_sorted(_: &mut Vm, a: &mut [Value]) -> VR {
    let kind = Kind::of_class(state_of(&a[0])?.borrow().class());
    let cmp = if kind == Kind::Double {
        // Double.compare order (NaN last, -0.0 before 0.0)
        Some(func::native(Some(cls::COMPARATOR), |_, a| Ok(Value::Int(func::java_double_cmp(double(&a[0]), double(&a[1])) as i32))))
    } else {
        None
    };
    with_stage(&a[0], Stage::Sorted(cmp), None, "sorted")
}
fn s_sorted_cmp(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Sorted(Some(a[1].clone())), None, "sortedCmp")
}
fn s_limit(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = long(&a[1]);
    if n < 0 {
        return Err(iae(n.to_string()));
    }
    with_stage(&a[0], Stage::Limit(n), None, "limit")
}
fn s_skip(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = long(&a[1]);
    if n < 0 {
        return Err(iae(n.to_string()));
    }
    if n == 0 {
        // `skip(0)` is the stream itself
        state_of(&a[0])?;
        return Ok(a[0].clone());
    }
    with_stage(&a[0], Stage::Skip(n), None, "skip")
}
fn s_boxed(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Convert(Kind::Ref), Some(Kind::Ref), "boxed")
}
fn s_as_long(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Convert(Kind::Long), Some(Kind::Long), "asLongStream")
}
fn s_as_double(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Convert(Kind::Double), Some(Kind::Double), "asDoubleStream")
}
fn s_identity(_: &mut Vm, a: &mut [Value]) -> VR {
    let _ = state_of(&a[0])?;
    Ok(a[0].clone())
}
fn s_is_parallel(_: &mut Vm, a: &mut [Value]) -> VR {
    let _ = state_of(&a[0])?;
    Ok(Value::Bool(false))
}
fn s_close(_: &mut Vm, a: &mut [Value]) -> VR {
    if let StreamState::Pipeline { used, .. } = &mut *state_of(&a[0])?.borrow_mut() {
        *used = true;
    }
    Ok(Value::Null)
}

// ------------------------------------------------------------------------------ terminal ops

fn s_for_each(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[1].clone();
    run(vm, &a[0], &mut |vm, x| {
        call(vm, &f, &[x])?;
        Ok(true)
    })?;
    Ok(Value::Null)
}
/// The exact size of a pipeline whose source is `SIZED` and whose stages keep the size known
/// (`count()` then answers without running the pipeline: no `peek` or `map` side effects, as
/// in JDK 9+).
fn known_size(st: &StreamState) -> Option<i64> {
    let StreamState::Pipeline { source, stages, .. } = st else { return None };
    let mut n = match source {
        Source::Items(items, _) => items.len() as i64,
        Source::List(l) => l.len() as i64,
        Source::Keys(m) => m.len() as i64,
        Source::Unsized(_) | Source::Split(..) | Source::Consumed { .. } => return None,
        Source::Concat(a, b) => known_size(a)?.checked_add(known_size(b)?)?,
    };
    for stage in stages {
        match stage {
            Stage::Map(..) | Stage::Peek(_) | Stage::Sorted(_) | Stage::Convert(_) => {}
            Stage::Limit(k) => n = n.min(*k),
            Stage::Skip(k) => n = (n - *k).max(0),
            Stage::Filter(_) | Stage::FlatMap(_) | Stage::Distinct => return None,
        }
    }
    Some(n)
}

fn s_count(vm: &mut Vm, a: &mut [Value]) -> VR {
    let known = match &*state_of(&a[0])?.borrow() {
        st @ StreamState::Pipeline { used: false, .. } => known_size(st),
        _ => None,
    };
    if let Some(n) = known {
        take(&a[0])?;
        return Ok(Value::Long(n));
    }
    let mut n = 0i64;
    run(vm, &a[0], &mut |_, _| {
        n += 1;
        Ok(true)
    })?;
    Ok(Value::Long(n))
}
fn s_match(vm: &mut Vm, a: &mut [Value], want: bool, stop_on: bool) -> VR {
    // anyMatch: stop on first true; allMatch: stop on first false; noneMatch: stop on first true
    let f = a[1].clone();
    let mut hit = false;
    run(vm, &a[0], &mut |vm, x| {
        let r = call_bool(vm, &f, &[x])?;
        if r == stop_on {
            hit = true;
            return Ok(false);
        }
        Ok(true)
    })?;
    Ok(Value::Bool(if hit { want } else { !want }))
}
fn s_any_match(vm: &mut Vm, a: &mut [Value]) -> VR {
    s_match(vm, a, true, true)
}
fn s_all_match(vm: &mut Vm, a: &mut [Value]) -> VR {
    s_match(vm, a, false, false)
}
fn s_none_match(vm: &mut Vm, a: &mut [Value]) -> VR {
    s_match(vm, a, false, true)
}
fn opt_class(k: Kind) -> ClassId {
    match k {
        Kind::Ref => cls::OPTIONAL,
        Kind::Int => cls::OPTIONAL_INT,
        Kind::Long => cls::OPTIONAL_LONG,
        Kind::Double => cls::OPTIONAL_DOUBLE,
    }
}
fn s_find_first(vm: &mut Vm, a: &mut [Value]) -> VR {
    let term = TERMINAL.with(|c| c.get());
    let mut found = None;
    let kind = run(vm, &a[0], &mut |_, x| {
        found = Some(x);
        Ok(false)
    })?;
    if let Some(Value::Null) = found {
        return Err(match term {
            Some(t) => null_result(&["java.util.stream.FindOps$FindSink$OfRef.get(FindOps.java:194)", "java.util.stream.FindOps$FindSink$OfRef.get(FindOps.java:191)"], t),
            None => Throw::new(cls::NULL_POINTER_EXCEPTION, None),
        });
    }
    Ok(crate::lib_util::optional(opt_class(kind), found))
}
/// `Optional.of(null)` of a reducing terminal's result (`reduce(op)`, `max`, `min`).
fn null_reduced(term: Option<TermInfo>) -> Thrown {
    match term {
        Some(t) => null_result(&["java.util.stream.ReduceOps$2ReducingSink.get(ReduceOps.java:129)", "java.util.stream.ReduceOps$2ReducingSink.get(ReduceOps.java:107)"], t),
        None => Throw::new(cls::NULL_POINTER_EXCEPTION, None),
    }
}

fn s_reduce1(vm: &mut Vm, a: &mut [Value]) -> VR {
    let term = TERMINAL.with(|c| c.get());
    let op = a[1].clone();
    let mut acc: Option<Value> = None;
    let kind = run(vm, &a[0], &mut |vm, x| {
        acc = Some(match acc.take() {
            None => x,
            Some(p) => call(vm, &op, &[p, x])?,
        });
        Ok(true)
    })?;
    let acc = match acc {
        Some(v) => Some(map_result(v, kind)?),
        None => None,
    };
    if let Some(Value::Null) = acc {
        return Err(null_reduced(term));
    }
    Ok(crate::lib_util::optional(opt_class(kind), acc))
}
fn s_reduce2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let op = a[2].clone();
    let mut acc = a[1].clone();
    let kind = Kind::of_class(state_of(&a[0])?.borrow().class());
    if kind != Kind::Ref {
        acc = kind.conv(acc);
    }
    run(vm, &a[0], &mut |vm, x| {
        let r = call(vm, &op, &[std::mem::take(&mut acc), x])?;
        acc = map_result(r, kind)?;
        Ok(true)
    })?;
    Ok(acc)
}
fn s_reduce3(vm: &mut Vm, a: &mut [Value]) -> VR {
    let op = a[2].clone();
    let mut acc = a[1].clone();
    run(vm, &a[0], &mut |vm, x| {
        acc = call(vm, &op, &[std::mem::take(&mut acc), x])?;
        Ok(true)
    })?;
    Ok(acc)
}
fn s_min_max(vm: &mut Vm, a: &mut [Value], max: bool) -> VR {
    let term = TERMINAL.with(|c| c.get());
    let cmp = a.get(1).cloned().unwrap_or(Value::Null);
    if a.len() > 1 && cmp.is_null() && Kind::of_class(state_of(&a[0])?.borrow().class()) == Kind::Ref {
        let (by, line, op, op_line) = if max { ("maxBy", 73, "max", 739) } else { ("minBy", 58, "min", 744) };
        return Err(Throw::new(cls::NULL_POINTER_EXCEPTION, None).with_frames(&[
            "java.base/java.util.Objects.requireNonNull(Objects.java:220)",
            &format!("java.base/java.util.function.BinaryOperator.{by}(BinaryOperator.java:{line})"),
            &format!("java.base/java.util.stream.ReferencePipeline.{op}(ReferencePipeline.java:{op_line})"),
        ]));
    }
    let mut best: Option<Value> = None;
    let kind = run(vm, &a[0], &mut |vm, x| {
        best = Some(match best.take() {
            None => x,
            Some(b) => {
                let o = if cmp.is_null() {
                    match (&b, &x) {
                        (Value::Double(p), Value::Double(q)) => func::java_double_cmp(*p, *q),
                        _ => func::natural_compare(&b, &x)?,
                    }
                } else {
                    func::compare_with(vm, &cmp, &b, &x).map_err(|mut e| {
                        // the comparison runs in `maxBy`/`minBy`'s lambda
                        if e.offset.is_none() && term.is_some() {
                            let by = if max { "java.base/java.util.function.BinaryOperator.lambda$maxBy$0(BinaryOperator.java:74)" } else { "java.base/java.util.function.BinaryOperator.lambda$minBy$0(BinaryOperator.java:59)" };
                            e.frames.push(by.into());
                            e.frames.push("java.base/java.util.stream.ReduceOps$2ReducingSink.accept(ReduceOps.java:123)".into());
                        }
                        e
                    })?
                };
                // `BinaryOperator.maxBy`/`minBy`: both keep the first of equal elements
                if max {
                    if o != std::cmp::Ordering::Less {
                        b
                    } else {
                        x
                    }
                } else if o == std::cmp::Ordering::Greater {
                    x
                } else {
                    b
                }
            }
        });
        Ok(true)
    })?;
    if kind == Kind::Double
        && let Some(Value::Double(d)) = &best {
            let _ = d;
        }
    if let Some(Value::Null) = best {
        return Err(null_reduced(term));
    }
    Ok(crate::lib_util::optional(opt_class(kind), best))
}
fn s_min(vm: &mut Vm, a: &mut [Value]) -> VR {
    s_min_max(vm, a, false)
}
fn s_max(vm: &mut Vm, a: &mut [Value]) -> VR {
    s_min_max(vm, a, true)
}
fn s_prim_min(vm: &mut Vm, a: &mut [Value]) -> VR {
    s_prim_minmax(vm, a, false)
}
fn s_prim_max(vm: &mut Vm, a: &mut [Value]) -> VR {
    s_prim_minmax(vm, a, true)
}
fn s_prim_minmax(vm: &mut Vm, a: &mut [Value], max: bool) -> VR {
    let (kind, items) = collect_all(vm, &a[0])?;
    let mut best: Option<Value> = None;
    for x in items {
        best = Some(match best {
            None => x,
            Some(b) => {
                let pick_x = match kind {
                    Kind::Double => {
                        let (p, q) = (double(&b), double(&x));
                        let r = if max { crate::lib_lang::java_max(p, q) } else { crate::lib_lang::java_min(p, q) };
                        if r.to_bits() == q.to_bits() && r.to_bits() != p.to_bits() {
                            true
                        } else {
                            r.is_nan() && !p.is_nan()
                        }
                    }
                    _ => {
                        let (p, q) = (long(&b), long(&x));
                        if max {
                            q > p
                        } else {
                            q < p
                        }
                    }
                };
                if pick_x {
                    x
                } else {
                    b
                }
            }
        });
    }
    Ok(crate::lib_util::optional(opt_class(kind), best))
}
fn s_sum(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (kind, items) = collect_all(vm, &a[0])?;
    Ok(match kind {
        Kind::Int => Value::Int(items.iter().fold(0i32, |s, x| s.wrapping_add(int(x)))),
        Kind::Long => Value::Long(items.iter().fold(0i64, |s, x| s.wrapping_add(long(x)))),
        _ => {
            let mut st = crate::lib_util::Stats::new(cls::DOUBLE_SUMMARY_STATISTICS);
            for x in &items {
                st.accept(x);
            }
            Value::Double(st.double_sum())
        }
    })
}
fn s_average(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (kind, items) = collect_all(vm, &a[0])?;
    if items.is_empty() {
        return Ok(crate::lib_util::optional(cls::OPTIONAL_DOUBLE, None));
    }
    let avg = match kind {
        Kind::Double => {
            let mut st = crate::lib_util::Stats::new(cls::DOUBLE_SUMMARY_STATISTICS);
            for x in &items {
                st.accept(x);
            }
            st.average()
        }
        _ => {
            let sum = items.iter().fold(0i64, |s, x| s.wrapping_add(long(x)));
            sum as f64 / items.len() as f64
        }
    };
    Ok(crate::lib_util::optional(cls::OPTIONAL_DOUBLE, Some(Value::Double(avg))))
}
fn s_summary(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (kind, items) = collect_all(vm, &a[0])?;
    let class = match kind {
        Kind::Int => cls::INT_SUMMARY_STATISTICS,
        Kind::Long => cls::LONG_SUMMARY_STATISTICS,
        _ => cls::DOUBLE_SUMMARY_STATISTICS,
    };
    let mut st = crate::lib_util::Stats::new(class);
    for x in &items {
        st.accept(x);
    }
    Ok(crate::lib_util::stats_value(st))
}
fn s_to_array(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (kind, items) = collect_all(vm, &a[0])?;
    let elem = match kind {
        Kind::Ref => Ty::DEF,
        Kind::Int => Ty::INT,
        Kind::Long => Ty::LONG,
        Kind::Double => Ty::DOUBLE,
    };
    let at = if elem == Ty::DEF { Ty::array(cls::OBJECT, 1) } else { Ty::array(elem.class, 1) };
    Ok(crate::dynamic::array_from(at, items))
}
/// `toArray(generator)`. A pipeline whose size is known asks the generator for the array
/// before the traversal and fills it in place (`Nodes.FixedNodeBuilder`: the array's length
/// must match exactly); otherwise the elements are buffered and copied into the generator's
/// array at the end (`SpinedBuffer.asArray`: it must be long enough).
fn s_to_array_gen(vm: &mut Vm, a: &mut [Value]) -> VR {
    let term = TERMINAL.with(|c| c.get());
    let g = a[1].clone();
    let known = match &*state_of(&a[0])?.borrow() {
        st @ StreamState::Pipeline { used: false, kind: Kind::Ref, .. } => known_size(st).map(|n| (n, begin_frames(st))),
        _ => None,
    };
    let method: &[&str] = term.map_or(&[], |t| t.method);
    let Some((n, begin)) = known else {
        let (_, items) = collect_all(vm, &a[0])?;
        let arr = call(vm, &g, &[Value::Int(items.len() as i32)])?;
        let spined = |site: &[&str]| {
            let mut f = site.to_vec();
            f.extend(["java.util.stream.SpinedBuffer.asArray(SpinedBuffer.java:215)", "java.util.stream.Nodes$SpinedNodeBuilder.asArray(Nodes.java:1304)", "java.util.stream.ReferencePipeline.toArray(ReferencePipeline.java:653)"]);
            f.extend(method.iter().skip(1));
            jdk(&f)
        };
        let Some(ja) = object_array(&arr) else {
            let mut e = not_object_array(&arr);
            let mut f = vec!["java.util.stream.SpinedBuffer.asArray(SpinedBuffer.java:214)", "java.util.stream.Nodes$SpinedNodeBuilder.asArray(Nodes.java:1304)", "java.util.stream.ReferencePipeline.toArray(ReferencePipeline.java:653)"];
            f.extend(method.iter().skip(1));
            e.frames = jdk(&f);
            return Err(e);
        };
        if ja.len() < items.len() {
            let mut e = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some("does not fit".into()));
            e.frames = spined(&["java.util.stream.SpinedBuffer.copyInto(SpinedBuffer.java:190)", "java.util.stream.Nodes$SpinedNodeBuilder.copyInto(Nodes.java:1298)"]);
            return Err(e);
        }
        for (i, x) in items.into_iter().enumerate() {
            if let Err(mut e) = crate::dynamic::array_store(&arr, &Value::Int(i as i32), x, None) {
                if e.class == cls::ARRAY_STORE_EXCEPTION {
                    // `System.arraycopy` of the buffer's `Object[]` chunk
                    e.message = Some(format!(
                        "arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, {}",
                        crate::lookup::java_binary_name(ja.elem.class)
                    ));
                    // (`System.arraycopy`'s own native frame is gone once the JIT intrinsifies it)
                    e.frames = spined(&["java.util.stream.SpinedBuffer.copyInto(SpinedBuffer.java:194)", "java.util.stream.Nodes$SpinedNodeBuilder.copyInto(Nodes.java:1298)"]);
                }
                return Err(e);
            }
        }
        return Ok(arr);
    };
    let n = n.min(i32::MAX as i64) as i32;
    let arr = call(vm, &g, &[Value::Int(n)])?;
    let builder = |site: &[&str], rest: Option<Vec<&'static str>>| -> Vec<String> {
        let Some(t) = term else { return Vec::new() };
        let Some(rest) = rest else { return Vec::new() };
        let mut f = site.to_vec();
        f.extend(rest);
        f.push(WRAP_AND_COPY);
        f.extend(t.eval);
        f.extend(t.method);
        jdk(&f)
    };
    if arr.is_null() {
        let mut e = npe("Cannot read the array length because \"this.array\" is null");
        e.frames = builder(&["java.util.stream.Nodes$FixedNodeBuilder.begin(Nodes.java:1221)"], begin);
        return Err(e);
    }
    let Some(len) = object_array(&arr).map(|ja| ja.len()) else {
        let mut e = not_object_array(&arr);
        if let Some(t) = term {
            let mut f = vec![
                "java.util.stream.Nodes$ArrayNode.<init>(Nodes.java:649)",
                "java.util.stream.Nodes$FixedNodeBuilder.<init>(Nodes.java:1207)",
                "java.util.stream.Nodes.builder(Nodes.java:169)",
                "java.util.stream.ReferencePipeline.makeNodeBuilder(ReferencePipeline.java:153)",
                "java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:633)",
                "java.util.stream.AbstractPipeline.evaluateToArrayNode(AbstractPipeline.java:291)",
            ];
            f.extend(t.method);
            e.frames = jdk(&f);
        }
        return Err(e);
    };
    if len != n as usize {
        let mut e = Throw::new(cls::ILLEGAL_STATE_EXCEPTION, Some(format!("Begin size {n} is not equal to fixed size {len}")));
        e.frames = builder(&["java.util.stream.Nodes$FixedNodeBuilder.begin(Nodes.java:1222)"], begin);
        return Err(e);
    }
    let mut i = 0i32;
    run(vm, &a[0], &mut |_, x| {
        crate::dynamic::array_store(&arr, &Value::Int(i), x, None).map_err(|mut e| {
            if e.class == cls::ARRAY_STORE_EXCEPTION {
                e.frames = jdk(&["java.util.stream.Nodes$FixedNodeBuilder.accept(Nodes.java:1230)"]);
            }
            e
        })?;
        i += 1;
        Ok(true)
    })?;
    Ok(arr)
}

/// The array of a reference type a generator returned (`Object[]` and its subtypes).
fn object_array(v: &Value) -> Option<&crate::value::JArray> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(ja) if !ja.elem.is_primitive() || ja.elem.dims > 0 => Some(ja),
            _ => None,
        },
        _ => None,
    }
}

/// The `ClassCastException` of a generator's result that is not an `Object[]`.
fn not_object_array(v: &Value) -> Thrown {
    let from = match v {
        Value::Obj(o) => match &**o {
            Obj::Array(ja) => ja.descriptor(),
            _ => crate::lookup::java_binary_name(v.class()).to_string(),
        },
        _ => crate::lookup::java_binary_name(v.class()).to_string(),
    };
    let to = "[Ljava.lang.Object;";
    crate::vm::cce(format!("class {from} cannot be cast to class {to} ({from} and {to} are in module java.base of loader 'bootstrap')"))
}

/// The frames between `FixedNodeBuilder.begin` and `wrapAndCopyInto`: `begin()` calls down
/// the chain of stage sinks (a sort barrier's downstream begins in its `end()`).
fn begin_frames(st: &StreamState) -> Option<Vec<&'static str>> {
    let StreamState::Pipeline { source, stages, .. } = st else { return None };
    if !matches!(source, Source::Items(..) | Source::List(_) | Source::Keys(_)) {
        return None;
    }
    let barrier = stages.iter().rposition(|s| matches!(s, Stage::Sorted(_)));
    let after = barrier.map_or(0, |b| b + 1);
    let mut f = Vec::new();
    for s in stages[after..].iter().rev() {
        f.push(match s {
            Stage::Map(_, Kind::Ref) | Stage::Peek(_) => "java.util.stream.Sink$ChainedReference.begin(Sink.java:256)",
            Stage::Limit(_) | Stage::Skip(_) => "java.util.stream.SliceOps$1$1.begin(SliceOps.java:192)",
            _ => return None,
        });
    }
    let short = stages.iter().any(|s| matches!(s, Stage::Limit(_)));
    match barrier {
        Some(b) => {
            f.push("java.util.stream.SortedOps$SizedRefSortingSink.end(SortedOps.java:354)");
            for s in stages[..b].iter().rev() {
                if !matches!(s, Stage::Map(_, Kind::Ref) | Stage::Peek(_)) {
                    return None;
                }
                f.push(CHAINED_END);
            }
            if short {
                f.extend(COPY_CANCEL_END);
            } else {
                f.push(COPY_INTO_END);
            }
        }
        None if short => f.extend(["java.util.stream.AbstractPipeline.copyIntoWithCancel(AbstractPipeline.java:587)", "java.util.stream.AbstractPipeline.copyInto(AbstractPipeline.java:574)"]),
        None => f.push("java.util.stream.AbstractPipeline.copyInto(AbstractPipeline.java:569)"),
    }
    Some(f)
}

fn s_iterator(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (kind, items) = collect_all(vm, &a[0])?;
    let class = match kind {
        Kind::Ref => cls::GENERIC_ITERATOR,
        Kind::Int => cls::PRIMITIVE_INT_ITERATOR,
        Kind::Long => cls::PRIMITIVE_LONG_ITERATOR,
        Kind::Double => cls::PRIMITIVE_DOUBLE_ITERATOR,
    };
    Ok(Value::obj(Obj::Iterator(RefCell::new(crate::iter::JIter::of_values(class, items)))))
}
fn s_collect3(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (sup, acc) = (a[1].clone(), a[2].clone());
    let container = call(vm, &sup, &[])?;
    let c2 = container.clone();
    run(vm, &a[0], &mut |vm, x| {
        call(vm, &acc, &[c2.clone(), x])?;
        Ok(true)
    })?;
    Ok(container)
}
fn s_collect(vm: &mut Vm, a: &mut [Value]) -> VR {
    let c = collector_of(&a[1])?;
    let mut st = CState::new(vm, &c)?;
    run(vm, &a[0], &mut |vm, x| {
        st.accept(vm, &c, x).map_err(|e| collector_site(e, &c))?;
        Ok(true)
    })?;
    st.finish(vm, &c)
}

// ------------------------------------------------------------------------------ sources

fn ss_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(stream_of(crate::lib_coll::array_values(&a[0])))
}
fn ss_empty(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(stream_of(Vec::new()))
}
fn prim_of(a: &mut [Value], class: ClassId) -> VR {
    Ok(prim_stream(class, crate::lib_coll::array_values(&a[0])))
}
fn is_of(_: &mut Vm, a: &mut [Value]) -> VR {
    prim_of(a, cls::INT_PIPELINE)
}
fn ls_of(_: &mut Vm, a: &mut [Value]) -> VR {
    prim_of(a, cls::LONG_PIPELINE)
}
fn ds_of(_: &mut Vm, a: &mut [Value]) -> VR {
    prim_of(a, cls::DOUBLE_PIPELINE)
}
fn is_empty_s(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(prim_stream(cls::INT_PIPELINE, Vec::new()))
}
fn ls_empty(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(prim_stream(cls::LONG_PIPELINE, Vec::new()))
}
fn ds_empty(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(prim_stream(cls::DOUBLE_PIPELINE, Vec::new()))
}
fn range_items(from: i64, to: i64, long_kind: bool) -> Result<Vec<Value>, Thrown> {
    if to <= from {
        return Ok(Vec::new());
    }
    // the range is materialized (Java's is lazy): it must fit like any other collection
    let n = (to as i128 - from as i128) as u128;
    crate::guard::check_elements(n.min(usize::MAX as u128) as usize)?;
    Ok((from..to).map(|i| if long_kind { Value::Long(i) } else { Value::Int(i as i32) }).collect())
}
fn is_range(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(prim_stream(cls::INT_PIPELINE, range_items(int(&a[0]) as i64, int(&a[1]) as i64, false)?))
}
fn is_range_closed(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(prim_stream(cls::INT_PIPELINE, range_items(int(&a[0]) as i64, int(&a[1]) as i64 + 1, false)?))
}
fn ls_range(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(prim_stream(cls::LONG_PIPELINE, range_items(long(&a[0]), long(&a[1]), true)?))
}
fn ls_range_closed(_: &mut Vm, a: &mut [Value]) -> VR {
    let to = long(&a[1]);
    Ok(prim_stream(cls::LONG_PIPELINE, range_items(long(&a[0]), to.saturating_add(1), true)?))
}
fn concat(a: &mut [Value]) -> VR {
    let (ka, sa, sta) = take(&a[0])?;
    let (_, sb, stb) = take(&a[1])?;
    let pa = StreamState::Pipeline { kind: ka, source: sa, stages: sta, used: false };
    let pb = StreamState::Pipeline { kind: ka, source: sb, stages: stb, used: false };
    Ok(stream_value(StreamState::Pipeline { kind: ka, source: Source::Concat(Box::new(pa), Box::new(pb)), stages: Vec::new(), used: false }))
}
fn s_concat(_: &mut Vm, a: &mut [Value]) -> VR {
    concat(a)
}
fn builder(kind: Kind) -> Value {
    stream_value(StreamState::Builder { kind, items: Vec::new(), built: false })
}
fn ss_builder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(builder(Kind::Ref))
}
fn is_builder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(builder(Kind::Int))
}
fn ls_builder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(builder(Kind::Long))
}
fn ds_builder(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(builder(Kind::Double))
}
fn b_add(_: &mut Vm, a: &mut [Value]) -> VR {
    {
        let mut s = state_of(&a[0])?.borrow_mut();
        let StreamState::Builder { kind, items, built } = &mut *s else { return Err(crate::vm::cce("not a builder")) };
        if *built {
            return Err(Throw::new(cls::ILLEGAL_STATE_EXCEPTION, None));
        }
        items.push(kind.conv(a[1].clone()));
    }
    Ok(a[0].clone())
}
fn b_build(_: &mut Vm, a: &mut [Value]) -> VR {
    let mut s = state_of(&a[0])?.borrow_mut();
    let StreamState::Builder { kind, items, built } = &mut *s else { return Err(crate::vm::cce("not a builder")) };
    if *built {
        return Err(Throw::new(cls::ILLEGAL_STATE_EXCEPTION, None));
    }
    *built = true;
    let items = std::mem::take(items);
    Ok(stream_value(StreamState::Pipeline { kind: *kind, source: Source::Items(items, Origin::Unknown), stages: Vec::new(), used: false }))
}


// ------------------------------------------------------------------------------ collectors

fn collector_of(v: &Value) -> Result<Rc<CollectorDef>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Collector(c) => Ok(c.clone()),
            _ => Err(crate::vm::cce("not a Collector")),
        },
        _ => Err(npe("Cannot invoke \"java.util.stream.Collector.supplier()\" because \"collector\" is null")),
    }
}

fn collector(c: CollectorDef) -> Value {
    Value::obj(Obj::Collector(Rc::new(c)))
}

/// Accumulation state of a collector.
enum CState {
    List(Vec<Value>),
    Set(Rc<JMap>),
    Map(Rc<JMap>),
    Joining(String, bool),
    Groups(Rc<JMap>, Vec<CState>),
    Partition([Option<Box<CState>>; 2]),
    Count(i64),
    Stats(crate::lib_util::Stats),
    Down(Box<CState>),
    Opt(Option<Value>),
    Acc(Value),
}

impl CState {
    fn new(vm: &mut Vm, c: &CollectorDef) -> Result<CState, Thrown> {
        Ok(match c {
            CollectorDef::ToList => CState::List(Vec::new()),
            CollectorDef::ToSet => CState::Set(Rc::new(JMap::new(cls::HASH_SET))),
            CollectorDef::ToMap { supplier, .. } => match supplier {
                Some(s) => {
                    let m = call(vm, s, &[])?;
                    match m {
                        Value::Map(m) => CState::Map(m),
                        _ => return Err(crate::vm::cce("supplier did not return a Map")),
                    }
                }
                None => CState::Map(Rc::new(JMap::hash_map())),
            },
            CollectorDef::Joining { .. } => CState::Joining(String::new(), false),
            CollectorDef::GroupingBy { factory, .. } => {
                let m = match factory {
                    Some(f) => match call(vm, f, &[])? {
                        Value::Map(m) => m,
                        _ => return Err(crate::vm::cce("map factory did not return a Map")),
                    },
                    None => Rc::new(JMap::hash_map()),
                };
                CState::Groups(m, Vec::new())
            }
            CollectorDef::PartitioningBy { down, .. } => {
                CState::Partition([Some(Box::new(CState::new(vm, down)?)), Some(Box::new(CState::new(vm, down)?))])
            }
            CollectorDef::Counting => CState::Count(0),
            CollectorDef::Summing(k, _) | CollectorDef::Averaging(k, _) | CollectorDef::Summarizing(k, _) => CState::Stats(crate::lib_util::Stats::new(match k {
                Kind::Int => cls::INT_SUMMARY_STATISTICS,
                Kind::Long => cls::LONG_SUMMARY_STATISTICS,
                _ => cls::DOUBLE_SUMMARY_STATISTICS,
            })),
            CollectorDef::Mapping(_, d) | CollectorDef::AndThen(d, _) => CState::Down(Box::new(CState::new(vm, d)?)),
            CollectorDef::MinMax { .. } => CState::Opt(None),
            CollectorDef::Reducing { identity, .. } => match identity {
                Some(i) => CState::Acc(i.clone()),
                None => CState::Opt(None),
            },
            CollectorDef::ToCollection(s) => CState::Acc(call(vm, s, &[])?),
            CollectorDef::Custom { supplier, .. } => CState::Acc(call(vm, supplier, &[])?),
        })
    }

    fn accept(&mut self, vm: &mut Vm, c: &CollectorDef, x: Value) -> Result<(), Thrown> {
        match (self, c) {
            (CState::List(v), _) => v.push(x),
            (CState::Set(s), _) => {
                s.insert_raw(x, Value::Null);
            }
            (CState::Map(m), CollectorDef::ToMap { k, v, merge, .. }) => {
                let key = call(vm, k, std::slice::from_ref(&x))?;
                let val = call(vm, v, std::slice::from_ref(&x))?;
                if val.is_null() {
                    return Err(Throw::new(cls::NULL_POINTER_EXCEPTION, None));
                }
                match m.get(&key) {
                    Some(old) => match merge {
                        Some(f) => {
                            let nv = call(vm, f, &[old, val])?;
                            if nv.is_null() {
                                m.remove(&key);
                            } else {
                                crate::lib_coll::map_put(vm, &Value::Map(m.clone()), key, nv)?;
                            }
                        }
                        None => {
                            return Err(Throw::new(
                                cls::ILLEGAL_STATE_EXCEPTION,
                                Some(format!("Duplicate key {} (attempted merging values {} and {})", key.to_java_string(), old.to_java_string(), val.to_java_string())),
                            ))
                        }
                    },
                    None => {
                        crate::lib_coll::map_put(vm, &Value::Map(m.clone()), key, val)?;
                    }
                }
            }
            (CState::Joining(s, any), CollectorDef::Joining { delim, .. }) => {
                let part = match &x {
                    Value::Null => "null".to_string(),
                    Value::Str(st) => st.as_str().to_string(),
                    other => match crate::lib_lang::char_seq(other) {
                        Ok(s) if matches!(other, Value::Obj(_)) => s,
                        _ => {
                            return Err(crate::vm::cce(crate::cast::checkcast_message(
                                crate::lookup::java_binary_name(other.class()),
                                "java.lang.CharSequence",
                            )))
                        }
                    },
                };
                if *any {
                    s.push_str(delim);
                }
                s.push_str(&part);
                *any = true;
            }
            (CState::Groups(m, states), CollectorDef::GroupingBy { classifier, down, .. }) => {
                let key = call(vm, classifier, std::slice::from_ref(&x))?;
                if key.is_null() {
                    return Err(npe("element cannot be mapped to a null key"));
                }
                let idx = match m.get(&key) {
                    Some(Value::Int(i)) => i as usize,
                    _ => {
                        states.push(CState::new(vm, down)?);
                        let i = states.len() - 1;
                        crate::lib_coll::map_put(vm, &Value::Map(m.clone()), key, Value::Int(i as i32))?;
                        i
                    }
                };
                states[idx].accept(vm, down, x)?;
            }
            (CState::Partition(parts), CollectorDef::PartitioningBy { pred, down }) => {
                let b = call_bool(vm, pred, std::slice::from_ref(&x))?;
                parts[b as usize].as_mut().unwrap().accept(vm, down, x)?;
            }
            (CState::Count(n), _) => *n += 1,
            (CState::Stats(st), CollectorDef::Summing(k, f) | CollectorDef::Averaging(k, f) | CollectorDef::Summarizing(k, f)) => {
                let r = call(vm, f, &[x])?;
                st.accept(&map_result(r, *k)?);
            }
            (CState::Down(d), CollectorDef::Mapping(f, down)) => {
                let r = call(vm, f, &[x])?;
                d.accept(vm, down, r)?;
            }
            (CState::Down(d), CollectorDef::AndThen(down, _)) => d.accept(vm, down, x)?,
            (CState::Opt(o), CollectorDef::MinMax { cmp, max }) => {
                *o = Some(match o.take() {
                    None => x,
                    Some(b) => {
                        let ord = func::compare_with(vm, cmp, &b, &x)?;
                        if *max {
                            if ord == std::cmp::Ordering::Less {
                                x
                            } else {
                                b
                            }
                        } else if ord == std::cmp::Ordering::Greater {
                            x
                        } else {
                            b
                        }
                    }
                });
            }
            (CState::Opt(o), CollectorDef::Reducing { mapper, op, .. }) => {
                let x = match mapper {
                    Some(m) => call(vm, m, &[x])?,
                    None => x,
                };
                *o = Some(match o.take() {
                    None => x,
                    Some(p) => call(vm, op, &[p, x])?,
                });
            }
            (CState::Acc(acc), CollectorDef::Reducing { mapper, op, .. }) => {
                let x = match mapper {
                    Some(m) => call(vm, m, &[x])?,
                    None => x,
                };
                *acc = call(vm, op, &[std::mem::take(acc), x])?;
            }
            (CState::Acc(acc), CollectorDef::ToCollection(_)) => {
                let target = acc.clone();
                crate::dynamic::call_named(vm, &target, "add", vec![x])?;
            }
            (CState::Acc(acc), CollectorDef::Custom { accumulator, .. }) => {
                call(vm, accumulator, &[acc.clone(), x])?;
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self, vm: &mut Vm, c: &CollectorDef) -> VR {
        Ok(match (self, c) {
            (CState::List(v), _) => JList::array_list(v),
            (CState::Set(s), _) => Value::Set(s),
            (CState::Map(m), _) => Value::Map(m),
            (CState::Joining(s, _), CollectorDef::Joining { prefix, suffix, .. }) => Value::string(format!("{prefix}{s}{suffix}")),
            (CState::Groups(m, states), CollectorDef::GroupingBy { down, .. }) => {
                let entries = m.entries();
                let mut finished: Vec<Option<Value>> = Vec::with_capacity(states.len());
                for st in states {
                    finished.push(Some(st.finish(vm, down)?));
                }
                for (k, idx) in entries {
                    let i = int(&idx) as usize;
                    let v = finished[i].take().unwrap_or(Value::Null);
                    m.data.borrow_mut().m.insert(Key(k), v);
                }
                Value::Map(m)
            }
            (CState::Partition(mut parts), CollectorDef::PartitioningBy { down, .. }) => {
                let f = parts[0].take().unwrap().finish(vm, down)?;
                let t = parts[1].take().unwrap().finish(vm, down)?;
                let m = JMap::new(cls::LINKED_HASH_MAP);
                m.insert_raw(Value::Bool(false), f);
                m.insert_raw(Value::Bool(true), t);
                Value::Map(Rc::new(m))
            }
            (CState::Count(n), _) => Value::Long(n),
            (CState::Stats(st), CollectorDef::Summing(k, _)) => match k {
                Kind::Int => Value::Int(st.sum as i32),
                Kind::Long => Value::Long(st.sum),
                _ => Value::Double(st.double_sum()),
            },
            (CState::Stats(st), CollectorDef::Averaging(..)) => Value::Double(st.average()),
            (CState::Stats(st), CollectorDef::Summarizing(..)) => crate::lib_util::stats_value(st),
            (CState::Down(d), CollectorDef::Mapping(_, down)) => d.finish(vm, down)?,
            (CState::Down(d), CollectorDef::AndThen(down, f)) => {
                let r = d.finish(vm, down)?;
                call(vm, f, &[r])?
            }
            (CState::Opt(o), _) => crate::lib_util::optional(cls::OPTIONAL, o),
            (CState::Acc(acc), CollectorDef::Custom { finisher: Some(f), .. }) => call(vm, f, &[acc])?,
            (CState::Acc(acc), _) => acc,
            _ => Value::Null,
        })
    }
}

fn c_to_list(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::ToList))
}
fn c_to_set(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::ToSet))
}
fn c_to_map(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::ToMap { k: a[0].clone(), v: a[1].clone(), merge: None, supplier: None }))
}
fn c_to_map3(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::ToMap { k: a[0].clone(), v: a[1].clone(), merge: Some(a[2].clone()), supplier: None }))
}
fn c_to_map4(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::ToMap { k: a[0].clone(), v: a[1].clone(), merge: Some(a[2].clone()), supplier: Some(a[3].clone()) }))
}
fn cs(v: &Value) -> Result<String, Thrown> {
    match v {
        Value::Null => Err(npe("")),
        other => crate::lib_lang::char_seq(other),
    }
}
fn c_joining(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Joining { delim: String::new(), prefix: String::new(), suffix: String::new() }))
}
fn c_joining1(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Joining { delim: cs(&a[0])?, prefix: String::new(), suffix: String::new() }))
}
fn c_joining3(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Joining { delim: cs(&a[0])?, prefix: cs(&a[1])?, suffix: cs(&a[2])? }))
}
fn c_grouping(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::GroupingBy { classifier: a[0].clone(), factory: None, down: Rc::new(CollectorDef::ToList) }))
}
fn c_grouping2(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::GroupingBy { classifier: a[0].clone(), factory: None, down: collector_of(&a[1])? }))
}
fn c_grouping3(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::GroupingBy { classifier: a[0].clone(), factory: Some(a[1].clone()), down: collector_of(&a[2])? }))
}
fn c_partitioning(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::PartitioningBy { pred: a[0].clone(), down: Rc::new(CollectorDef::ToList) }))
}
fn c_partitioning2(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::PartitioningBy { pred: a[0].clone(), down: collector_of(&a[1])? }))
}
fn c_counting(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Counting))
}
macro_rules! numeric_collector {
    ($name:ident, $variant:ident, $kind:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            Ok(collector(CollectorDef::$variant($kind, a[0].clone())))
        }
    };
}
numeric_collector!(c_summing_int, Summing, Kind::Int);
numeric_collector!(c_summing_long, Summing, Kind::Long);
numeric_collector!(c_summing_double, Summing, Kind::Double);
numeric_collector!(c_averaging_int, Averaging, Kind::Int);
numeric_collector!(c_averaging_long, Averaging, Kind::Long);
numeric_collector!(c_averaging_double, Averaging, Kind::Double);
numeric_collector!(c_summarizing_int, Summarizing, Kind::Int);
numeric_collector!(c_summarizing_long, Summarizing, Kind::Long);
numeric_collector!(c_summarizing_double, Summarizing, Kind::Double);
fn c_mapping(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Mapping(a[0].clone(), collector_of(&a[1])?)))
}
fn c_min_by(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::MinMax { cmp: a[0].clone(), max: false }))
}
fn c_max_by(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::MinMax { cmp: a[0].clone(), max: true }))
}
fn c_reducing1(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Reducing { identity: None, mapper: None, op: a[0].clone() }))
}
fn c_reducing2(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Reducing { identity: Some(a[0].clone()), mapper: None, op: a[1].clone() }))
}
fn c_reducing3(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Reducing { identity: Some(a[0].clone()), mapper: Some(a[1].clone()), op: a[2].clone() }))
}
fn c_to_collection(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::ToCollection(a[0].clone())))
}
fn c_and_then(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::AndThen(collector_of(&a[0])?, a[1].clone())))
}
fn c_of4(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Custom { supplier: a[0].clone(), accumulator: a[1].clone(), combiner: a[2].clone(), finisher: None }))
}
fn c_of5(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(collector(CollectorDef::Custom { supplier: a[0].clone(), accumulator: a[1].clone(), combiner: a[2].clone(), finisher: Some(a[3].clone()) }))
}
fn coll_part(a: &mut [Value], f: impl FnOnce(&CollectorDef) -> Value) -> VR {
    let c = collector_of(&a[0])?;
    Ok(f(&c))
}
fn c_supplier(_: &mut Vm, a: &mut [Value]) -> VR {
    coll_part(a, |c| match c {
        CollectorDef::Custom { supplier, .. } => supplier.clone(),
        _ => func::native(Some(cls::SUPPLIER), |_, _| Ok(JList::array_list(Vec::new()))),
    })
}
fn c_accumulator(_: &mut Vm, a: &mut [Value]) -> VR {
    coll_part(a, |c| match c {
        CollectorDef::Custom { accumulator, .. } => accumulator.clone(),
        _ => Value::Null,
    })
}
fn c_combiner(_: &mut Vm, a: &mut [Value]) -> VR {
    coll_part(a, |c| match c {
        CollectorDef::Custom { combiner, .. } => combiner.clone(),
        _ => Value::Null,
    })
}
fn c_finisher(_: &mut Vm, a: &mut [Value]) -> VR {
    coll_part(a, |c| match c {
        CollectorDef::Custom { finisher: Some(f), .. } => f.clone(),
        _ => func::native(Some(cls::FUNCTION), |_, a| Ok(a.first().cloned().unwrap_or(Value::Null))),
    })
}
fn c_characteristics(_: &mut Vm, a: &mut [Value]) -> VR {
    let _ = collector_of(&a[0])?;
    Ok(Value::Set(Rc::new(JMap::new(cls::HASH_SET))))
}
fn ch_value(name: &str) -> Value {
    Value::str(name)
}
fn ch_concurrent(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(ch_value("CONCURRENT"))
}
fn ch_identity(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(ch_value("IDENTITY_FINISH"))
}
fn ch_unordered(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(ch_value("UNORDERED"))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.interface(BASE_STREAM, &[]);
    b.interface(STREAM, &[BASE_STREAM]);
    b.interface(INT_STREAM, &[BASE_STREAM]);
    b.interface(LONG_STREAM, &[BASE_STREAM]);
    b.interface(DOUBLE_STREAM, &[BASE_STREAM]);
    b.supers(REFERENCE_PIPELINE, &[OBJECT, STREAM]);
    b.supers(INT_PIPELINE, &[OBJECT, INT_STREAM]);
    b.supers(LONG_PIPELINE, &[OBJECT, LONG_STREAM]);
    b.supers(DOUBLE_PIPELINE, &[OBJECT, DOUBLE_STREAM]);
    b.interface(STREAM_BUILDER, &[CONSUMER]);
    b.interface(INT_STREAM_BUILDER, &[INT_CONSUMER]);
    b.interface(LONG_STREAM_BUILDER, &[LONG_CONSUMER]);
    b.interface(DOUBLE_STREAM_BUILDER, &[DOUBLE_CONSUMER]);
    b.supers(STREAM_BUILDER_IMPL, &[OBJECT, STREAM_BUILDER]);
    b.supers(INT_STREAM_BUILDER_IMPL, &[OBJECT, INT_STREAM_BUILDER]);
    b.supers(LONG_STREAM_BUILDER_IMPL, &[OBJECT, LONG_STREAM_BUILDER]);
    b.supers(DOUBLE_STREAM_BUILDER_IMPL, &[OBJECT, DOUBLE_STREAM_BUILDER]);
    b.interface(COLLECTOR, &[]);
    b.supers(COLLECTOR_IMPL, &[OBJECT, COLLECTOR]);
    b.supers(COLLECTORS, &[OBJECT]);
    b.supers(COLLECTOR_CHARACTERISTICS, &[ENUM]);

    b.m(BASE_STREAM, "void close()", s_close);
    b.m(BASE_STREAM, "boolean isParallel()", s_is_parallel);
    b.m(BASE_STREAM, "Iterator iterator()", p_iterator);
    b.m(BASE_STREAM, "BaseStream sequential()", s_identity);
    b.m(BASE_STREAM, "BaseStream unordered()", s_identity);

    // Stream
    b.m(STREAM, "boolean allMatch(Predicate)", t_all_match);
    b.m(STREAM, "boolean anyMatch(Predicate)", t_any_match);
    b.s(STREAM, "Stream.Builder builder()", ss_builder);
    b.m(STREAM, "def collect(Collector)", t_collect);
    b.m(STREAM, "def collect(Supplier,BiConsumer,BiConsumer)", t_collect3);
    b.s(STREAM, "Stream concat(Stream,Stream)", s_concat);
    b.m(STREAM, "long count()", t_count);
    b.m(STREAM, "Stream distinct()", s_distinct);
    b.s(STREAM, "Stream empty()", ss_empty);
    b.m(STREAM, "Stream filter(Predicate)", s_filter);
    b.m(STREAM, "Optional findAny()", t_find_any);
    b.m(STREAM, "Optional findFirst()", t_find_first);
    b.m(STREAM, "Stream flatMap(Function)", s_flat_map);
    b.m(STREAM, "DoubleStream flatMapToDouble(Function)", s_flat_map_to_double);
    b.m(STREAM, "IntStream flatMapToInt(Function)", s_flat_map_to_int);
    b.m(STREAM, "LongStream flatMapToLong(Function)", s_flat_map_to_long);
    b.m(STREAM, "void forEach(Consumer)", t_for_each);
    b.m(STREAM, "void forEachOrdered(Consumer)", t_for_each_ordered);
    b.m(STREAM, "Stream limit(long)", s_limit);
    b.m(STREAM, "Stream map(Function)", s_map);
    b.m(STREAM, "DoubleStream mapToDouble(ToDoubleFunction)", s_map_to_double);
    b.m(STREAM, "IntStream mapToInt(ToIntFunction)", s_map_to_int);
    b.m(STREAM, "LongStream mapToLong(ToLongFunction)", s_map_to_long);
    b.m(STREAM, "Optional max(Comparator)", t_max);
    b.m(STREAM, "Optional min(Comparator)", t_min);
    b.m(STREAM, "boolean noneMatch(Predicate)", t_none_match);
    b.s(STREAM, "Stream of(def[])", ss_of);
    b.m(STREAM, "Stream peek(Consumer)", s_peek);
    b.m(STREAM, "Optional reduce(BinaryOperator)", t_reduce1);
    b.m(STREAM, "def reduce(def,BinaryOperator)", t_reduce2);
    b.m(STREAM, "def reduce(def,BiFunction,BinaryOperator)", t_reduce3);
    b.m(STREAM, "Stream skip(long)", s_skip);
    b.m(STREAM, "Stream sorted()", s_sorted);
    b.m(STREAM, "Stream sorted(Comparator)", s_sorted_cmp);
    b.m(STREAM, "def[] toArray()", t_to_array);
    b.m(STREAM, "def[] toArray(IntFunction)", t_to_array_gen);
    b.m(STREAM_BUILDER, "Stream.Builder add(def)", b_add);
    b.m(STREAM_BUILDER, "Stream build()", b_build);
    b.m(STREAM_BUILDER, "void accept(def)", b_add);

    // primitive streams
    for (c, opt, bld, consumer_sig, prim, arr) in [
        (INT_STREAM, "OptionalInt", "IntStream.Builder", "IntConsumer", "int", "int[]"),
        (LONG_STREAM, "OptionalLong", "LongStream.Builder", "LongConsumer", "long", "long[]"),
        (DOUBLE_STREAM, "OptionalDouble", "DoubleStream.Builder", "DoubleConsumer", "double", "double[]"),
    ] {
        let name = match c {
            INT_STREAM => "Int",
            LONG_STREAM => "Long",
            _ => "Double",
        };
        let me = format!("{name}Stream");
        b.m(c, &format!("boolean allMatch({name}Predicate)"), p_all_match);
        b.m(c, &format!("boolean anyMatch({name}Predicate)"), p_any_match);
        b.m(c, "OptionalDouble average()", p_average);
        b.m(c, "Stream boxed()", s_boxed);
        b.m(c, &format!("def collect(Supplier,Obj{name}Consumer,BiConsumer)"), p_collect3);
        b.s(c, &format!("{me} concat({me},{me})"), s_concat);
        b.m(c, "long count()", p_count);
        b.m(c, &format!("{me} distinct()"), s_distinct);
        b.m(c, &format!("{me} filter({name}Predicate)"), s_filter);
        b.m(c, &format!("{opt} findAny()"), p_find_any);
        b.m(c, &format!("{opt} findFirst()"), p_find_first);
        b.m(c, &format!("{me} flatMap({name}Function)"), s_flat_map);
        b.m(c, &format!("void forEach({consumer_sig})"), p_for_each);
        b.m(c, &format!("void forEachOrdered({consumer_sig})"), p_for_each_ordered);
        b.m(c, &format!("PrimitiveIterator.Of{name} iterator()"), p_iterator);
        b.m(c, &format!("{me} limit(long)"), s_limit);
        b.m(c, &format!("{me} map({name}UnaryOperator)"), s_map);
        b.m(c, &format!("Stream mapToObj({name}Function)"), s_map_to_obj);
        b.m(c, &format!("{opt} max()"), p_max);
        b.m(c, &format!("{opt} min()"), p_min);
        b.m(c, &format!("boolean noneMatch({name}Predicate)"), p_none_match);
        b.m(c, &format!("{me} peek({consumer_sig})"), s_peek);
        b.m(c, &format!("{opt} reduce({name}BinaryOperator)"), p_reduce1);
        b.m(c, &format!("{prim} reduce({prim},{name}BinaryOperator)"), p_reduce2);
        b.m(c, &format!("{me} sequential()"), s_identity);
        b.m(c, &format!("{me} skip(long)"), s_skip);
        b.m(c, &format!("{me} sorted()"), s_sorted);
        b.m(c, &format!("{prim} sum()"), p_sum);
        b.m(c, &format!("{name}SummaryStatistics summaryStatistics()"), p_summary);
        b.m(c, &format!("{arr} toArray()"), p_to_array);
        let _ = bld;
    }
    b.s(INT_STREAM, "IntStream.Builder builder()", is_builder);
    b.s(LONG_STREAM, "LongStream.Builder builder()", ls_builder);
    b.s(DOUBLE_STREAM, "DoubleStream.Builder builder()", ds_builder);
    b.s(INT_STREAM, "IntStream empty()", is_empty_s);
    b.s(LONG_STREAM, "LongStream empty()", ls_empty);
    b.s(DOUBLE_STREAM, "DoubleStream empty()", ds_empty);
    b.s(INT_STREAM, "IntStream of(int[])", is_of);
    b.s(LONG_STREAM, "LongStream of(long[])", ls_of);
    b.s(DOUBLE_STREAM, "DoubleStream of(double[])", ds_of);
    b.s(INT_STREAM, "IntStream range(int,int)", is_range);
    b.s(INT_STREAM, "IntStream rangeClosed(int,int)", is_range_closed);
    b.s(LONG_STREAM, "LongStream range(long,long)", ls_range);
    b.s(LONG_STREAM, "LongStream rangeClosed(long,long)", ls_range_closed);
    b.m(INT_STREAM, "DoubleStream asDoubleStream()", s_as_double);
    b.m(INT_STREAM, "LongStream asLongStream()", s_as_long);
    b.m(INT_STREAM, "DoubleStream mapToDouble(IntToDoubleFunction)", s_map_to_double);
    b.m(INT_STREAM, "LongStream mapToLong(IntToLongFunction)", s_map_to_long);
    b.m(LONG_STREAM, "DoubleStream asDoubleStream()", s_as_double);
    b.m(LONG_STREAM, "DoubleStream mapToDouble(LongToDoubleFunction)", s_map_to_double);
    b.m(LONG_STREAM, "IntStream mapToInt(LongToIntFunction)", s_map_to_int);
    b.m(DOUBLE_STREAM, "IntStream mapToInt(DoubleToIntFunction)", s_map_to_int);
    b.m(DOUBLE_STREAM, "LongStream mapToLong(DoubleToLongFunction)", s_map_to_long);
    b.m(INT_STREAM_BUILDER, "IntStream.Builder add(int)", b_add);
    b.m(INT_STREAM_BUILDER, "IntStream build()", b_build);
    b.m(LONG_STREAM_BUILDER, "LongStream.Builder add(long)", b_add);
    b.m(LONG_STREAM_BUILDER, "LongStream build()", b_build);
    b.m(DOUBLE_STREAM_BUILDER, "DoubleStream.Builder add(double)", b_add);
    b.m(DOUBLE_STREAM_BUILDER, "DoubleStream build()", b_build);

    // Collector / Collectors
    b.m(COLLECTOR, "BiConsumer accumulator()", c_accumulator);
    b.m(COLLECTOR, "Set characteristics()", c_characteristics);
    b.m(COLLECTOR, "BinaryOperator combiner()", c_combiner);
    b.m(COLLECTOR, "Function finisher()", c_finisher);
    b.m(COLLECTOR, "Supplier supplier()", c_supplier);
    b.s(COLLECTOR, "Collector of(Supplier,BiConsumer,BinaryOperator,Function,Collector.Characteristics[])", c_of5);
    b.s(COLLECTOR, "Collector of(Supplier,BiConsumer,BinaryOperator,Collector.Characteristics[])", c_of4);
    b.sf(COLLECTOR_CHARACTERISTICS, "Collector.Characteristics CONCURRENT", ch_concurrent);
    b.sf(COLLECTOR_CHARACTERISTICS, "Collector.Characteristics IDENTITY_FINISH", ch_identity);
    b.sf(COLLECTOR_CHARACTERISTICS, "Collector.Characteristics UNORDERED", ch_unordered);
    b.s(COLLECTORS, "Collector averagingDouble(ToDoubleFunction)", c_averaging_double);
    b.s(COLLECTORS, "Collector averagingInt(ToIntFunction)", c_averaging_int);
    b.s(COLLECTORS, "Collector averagingLong(ToLongFunction)", c_averaging_long);
    b.s(COLLECTORS, "Collector collectingAndThen(Collector,Function)", c_and_then);
    b.s(COLLECTORS, "Collector counting()", c_counting);
    b.s(COLLECTORS, "Collector groupingBy(Function)", c_grouping);
    b.s(COLLECTORS, "Collector groupingBy(Function,Collector)", c_grouping2);
    b.s(COLLECTORS, "Collector groupingBy(Function,Supplier,Collector)", c_grouping3);
    b.s(COLLECTORS, "Collector joining()", c_joining);
    b.s(COLLECTORS, "Collector joining(CharSequence)", c_joining1);
    b.s(COLLECTORS, "Collector joining(CharSequence,CharSequence,CharSequence)", c_joining3);
    b.s(COLLECTORS, "Collector mapping(Function,Collector)", c_mapping);
    b.s(COLLECTORS, "Collector maxBy(Comparator)", c_max_by);
    b.s(COLLECTORS, "Collector minBy(Comparator)", c_min_by);
    b.s(COLLECTORS, "Collector partitioningBy(Predicate)", c_partitioning);
    b.s(COLLECTORS, "Collector partitioningBy(Predicate,Collector)", c_partitioning2);
    b.s(COLLECTORS, "Collector reducing(BinaryOperator)", c_reducing1);
    b.s(COLLECTORS, "Collector reducing(def,BinaryOperator)", c_reducing2);
    b.s(COLLECTORS, "Collector reducing(def,Function,BinaryOperator)", c_reducing3);
    b.s(COLLECTORS, "Collector summarizingDouble(ToDoubleFunction)", c_summarizing_double);
    b.s(COLLECTORS, "Collector summarizingInt(ToIntFunction)", c_summarizing_int);
    b.s(COLLECTORS, "Collector summarizingLong(ToLongFunction)", c_summarizing_long);
    b.s(COLLECTORS, "Collector summingDouble(ToDoubleFunction)", c_summing_double);
    b.s(COLLECTORS, "Collector summingInt(ToIntFunction)", c_summing_int);
    b.s(COLLECTORS, "Collector summingLong(ToLongFunction)", c_summing_long);
    b.s(COLLECTORS, "Collector toCollection(Supplier)", c_to_collection);
    b.s(COLLECTORS, "Collector toList()", c_to_list);
    b.s(COLLECTORS, "Collector toMap(Function,Function)", c_to_map);
    b.s(COLLECTORS, "Collector toMap(Function,Function,BinaryOperator)", c_to_map3);
    b.s(COLLECTORS, "Collector toMap(Function,Function,BinaryOperator,Supplier)", c_to_map4);
    b.s(COLLECTORS, "Collector toSet()", c_to_set);
}
