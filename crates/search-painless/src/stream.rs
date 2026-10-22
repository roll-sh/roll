//! `java.util.stream`: `Stream`, `IntStream`, `LongStream`, `DoubleStream`, their builders, and
//! `Collectors`.
//!
//! A stream is a source plus a list of pending stages; intermediate operations move the pipeline
//! into a new stream object (the old one becomes "linked", so reusing it fails like Java's) and
//! terminal operations run it. Running pushes each source element through the stages, which
//! keeps Java's laziness where scripts can see it: `peek` and `map` run interleaved per element,
//! `limit`, `findFirst` and the `*Match` operations stop the source early, and `sorted`/`distinct`
//! act as barriers.

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
    /// Elements of a `SIZED` spliterator (collections, arrays, ranges, builders).
    Items(Vec<Value>),
    /// Elements of a spliterator without a known size (`Pattern.splitAsStream`).
    Unsized(Vec<Value>),
    Concat(Box<StreamState>, Box<StreamState>),
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

pub fn stream_value(s: StreamState) -> Value {
    Value::obj(Obj::Stream(RefCell::new(s)))
}

/// A `Stream` over the given elements.
pub fn stream_of(items: Vec<Value>) -> Value {
    stream_value(StreamState::Pipeline { kind: Kind::Ref, source: Source::Items(items), stages: Vec::new(), used: false })
}

/// A reference stream whose source does not report its size (`Pattern.splitAsStream`).
pub fn unsized_stream_of(items: Vec<Value>) -> Value {
    stream_value(StreamState::Pipeline { kind: Kind::Ref, source: Source::Unsized(items), stages: Vec::new(), used: false })
}

/// A primitive stream (`IntStream` etc. by pipeline class) over the given elements.
pub fn prim_stream(class: ClassId, items: Vec<Value>) -> Value {
    stream_value(StreamState::Pipeline { kind: Kind::of_class(class), source: Source::Items(items), stages: Vec::new(), used: false })
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
/// `AbstractPipeline.evaluate`, called from the operation at `line` of ReferencePipeline.java.
fn terminal(vm: &mut Vm, a: &mut [Value], op: &str, line: u32, f: fn(&mut Vm, &mut [Value]) -> VR) -> VR {
    let reference = a[0].class() == cls::REFERENCE_PIPELINE;
    f(vm, a).map_err(|mut e| {
        if reference && e.class == cls::ILLEGAL_STATE_EXCEPTION && e.frames.is_empty() && e.offset.is_none() && e.message.as_deref() == Some("stream has already been operated upon or closed") {
            e.frames = vec![
                "java.base/java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:260)".into(),
                format!("java.base/java.util.stream.ReferencePipeline.{op}(ReferencePipeline.java:{line})"),
            ];
        }
        e
    })
}

macro_rules! terminal_op {
    ($name:ident, $inner:ident, $op:literal, $line:literal) => {
        fn $name(vm: &mut Vm, a: &mut [Value]) -> VR {
            terminal(vm, a, $op, $line, $inner)
        }
    };
}
terminal_op!(t_all_match, s_all_match, "allMatch", 673);
terminal_op!(t_any_match, s_any_match, "anyMatch", 668);
terminal_op!(t_none_match, s_none_match, "noneMatch", 678);
terminal_op!(t_collect, s_collect, "collect", 723);
terminal_op!(t_collect3, s_collect3, "collect", 734);
terminal_op!(t_count, s_count, "count", 750);
terminal_op!(t_find_any, s_find_first, "findAny", 688);
terminal_op!(t_find_first, s_find_first, "findFirst", 683);
terminal_op!(t_for_each, s_for_each, "forEach", 632);
terminal_op!(t_for_each_ordered, s_for_each, "forEachOrdered", 637);
terminal_op!(t_reduce1, s_reduce1, "reduce", 698);
terminal_op!(t_reduce2, s_reduce2, "reduce", 693);
terminal_op!(t_reduce3, s_reduce3, "reduce", 703);

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
            let src = std::mem::replace(source, Source::Items(Vec::new()));
            Ok((*kind, src, std::mem::take(stages)))
        }
        StreamState::Builder { .. } => Err(crate::vm::cce("not a stream")),
    }
}

fn with_stage(v: &Value, stage: Stage, new_kind: Option<Kind>) -> VR {
    let (kind, source, mut stages) = take(v)?;
    stages.push(stage);
    Ok(stream_value(StreamState::Pipeline { kind: new_kind.unwrap_or(kind), source, stages, used: false }))
}

// ------------------------------------------------------------------------------ evaluation

type Sink<'s> = dyn FnMut(&mut Vm, Value) -> Result<bool, Thrown> + 's;

struct Run {
    stages: Vec<Stage>,
    /// Per-stage state: counters for limit/skip, buffers for sorted, seen sets for distinct.
    counters: Vec<i64>,
    buffers: Vec<Vec<Value>>,
    seen: Vec<HashSet<Key>>,
    /// Stage index at which the pipeline stopped accepting input.
    stopped: bool,
}

impl Run {
    fn new(stages: Vec<Stage>) -> Run {
        let n = stages.len();
        Run { stages, counters: vec![0; n], buffers: (0..n).map(|_| Vec::new()).collect(), seen: (0..n).map(|_| HashSet::new()).collect(), stopped: false }
    }

    /// Pushes `v` into stage `k`; `Ok(false)` means downstream wants no more elements.
    fn push(&mut self, vm: &mut Vm, k: usize, v: Value, sink: &mut Sink) -> Result<bool, Thrown> {
        if k == self.stages.len() {
            return sink(vm, v);
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
                let cmp = cmp.clone().unwrap_or(Value::Null);
                let mut items = std::mem::take(&mut self.buffers[k]);
                func::sort_values(vm, &mut items, &cmp)?;
                for it in items {
                    if !self.push(vm, k + 1, it, sink)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn drive(&mut self, vm: &mut Vm, src: Source, sink: &mut Sink) -> Result<(), Thrown> {
        match src {
            Source::Items(items) | Source::Unsized(items) => {
                for it in items {
                    if !self.push(vm, 0, it, sink)? {
                        self.stopped = true;
                        break;
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
    let (kind, src, stages) = take(v)?;
    let mut r = Run::new(stages);
    r.drive(vm, src, sink)?;
    Ok(kind)
}

fn collect_all(vm: &mut Vm, v: &Value) -> Result<(Kind, Vec<Value>), Thrown> {
    let mut out = Vec::new();
    let kind = run(vm, v, &mut |_, x| {
        out.push(x);
        Ok(true)
    })?;
    Ok((kind, out))
}

// ------------------------------------------------------------------------------ intermediate ops

fn s_filter(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Filter(a[1].clone()), None)
}
fn s_map(_: &mut Vm, a: &mut [Value]) -> VR {
    let kind = Kind::of_class(state_of(&a[0])?.borrow().class());
    with_stage(&a[0], Stage::Map(a[1].clone(), kind), None)
}
fn map_to(a: &mut [Value], k: Kind) -> VR {
    with_stage(&a[0], Stage::Map(a[1].clone(), k), Some(k))
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
    with_stage(&a[0], Stage::FlatMap(a[1].clone()), None)
}
fn s_flat_map_to(a: &mut [Value], k: Kind) -> VR {
    with_stage(&a[0], Stage::FlatMap(a[1].clone()), Some(k))
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
    with_stage(&a[0], Stage::Peek(a[1].clone()), None)
}
fn s_distinct(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Distinct, None)
}
fn s_sorted(_: &mut Vm, a: &mut [Value]) -> VR {
    let kind = Kind::of_class(state_of(&a[0])?.borrow().class());
    let cmp = if kind == Kind::Double {
        // Double.compare order (NaN last, -0.0 before 0.0)
        Some(func::native(Some(cls::COMPARATOR), |_, a| Ok(Value::Int(func::java_double_cmp(double(&a[0]), double(&a[1])) as i32))))
    } else {
        None
    };
    with_stage(&a[0], Stage::Sorted(cmp), None)
}
fn s_sorted_cmp(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Sorted(Some(a[1].clone())), None)
}
fn s_limit(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = long(&a[1]);
    if n < 0 {
        return Err(iae(n.to_string()));
    }
    with_stage(&a[0], Stage::Limit(n), None)
}
fn s_skip(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = long(&a[1]);
    if n < 0 {
        return Err(iae(n.to_string()));
    }
    with_stage(&a[0], Stage::Skip(n), None)
}
fn s_boxed(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Convert(Kind::Ref), Some(Kind::Ref))
}
fn s_as_long(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Convert(Kind::Long), Some(Kind::Long))
}
fn s_as_double(_: &mut Vm, a: &mut [Value]) -> VR {
    with_stage(&a[0], Stage::Convert(Kind::Double), Some(Kind::Double))
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
        Source::Items(items) => items.len() as i64,
        Source::Unsized(_) => return None,
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
    let mut found = None;
    let kind = run(vm, &a[0], &mut |_, x| {
        found = Some(x);
        Ok(false)
    })?;
    if let Some(Value::Null) = found {
        return Err(npe(""));
    }
    Ok(crate::lib_util::optional(opt_class(kind), found))
}
fn s_reduce1(vm: &mut Vm, a: &mut [Value]) -> VR {
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
        return Err(npe(""));
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
    let cmp = a.get(1).cloned().unwrap_or(Value::Null);
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
                    func::compare_with(vm, &cmp, &b, &x)?
                };
                // min keeps the first of equal elements, max the last
                if max {
                    if o == std::cmp::Ordering::Greater {
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
        return Err(npe(""));
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
fn s_to_array_gen(vm: &mut Vm, a: &mut [Value]) -> VR {
    let (_, items) = collect_all(vm, &a[0])?;
    let g = a[1].clone();
    let arr = call(vm, &g, &[Value::Int(items.len() as i32)])?;
    let Value::Obj(o) = &arr else { return Err(npe("")) };
    let Obj::Array(ja) = &**o else { return Err(crate::vm::cce("not an array")) };
    if ja.len() < items.len() {
        return Err(Throw::new(cls::ILLEGAL_STATE_EXCEPTION, Some(format!("Begin size {} is not equal to fixed size {}", items.len(), ja.len()))));
    }
    for (i, x) in items.into_iter().enumerate() {
        crate::dynamic::array_store(&arr, &Value::Int(i as i32), x, None)?;
    }
    Ok(arr)
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
    // `list.stream().collect(toMap(…))` reports the JDK's pipeline frames on duplicate keys
    let plain_list = matches!(&*state_of(&a[0])?.borrow(), StreamState::Pipeline { source: Source::Items(_), stages, .. } if stages.is_empty())
        && matches!(&*c, CollectorDef::ToMap { merge: None, .. });
    let mut st = CState::new(vm, &c)?;
    run(vm, &a[0], &mut |vm, x| {
        st.accept(vm, &c, x)?;
        Ok(true)
    })
    .map_err(|mut t| {
        if plain_list && t.frames.is_empty() && t.message.as_deref().is_some_and(|m| m.starts_with("Duplicate key")) {
            t.frames = [
                "java.util.stream.Collectors.duplicateKeyException(Collectors.java:137)",
                "java.util.stream.Collectors.lambda$uniqKeysMapAccumulator$0(Collectors.java:184)",
                "java.util.stream.ReduceOps$3ReducingSink.accept(ReduceOps.java:169)",
                "java.util.ArrayList$ArrayListSpliterator.forEachRemaining(ArrayList.java:1724)",
                "java.util.stream.AbstractPipeline.copyInto(AbstractPipeline.java:570)",
                "java.util.stream.AbstractPipeline.wrapAndCopyInto(AbstractPipeline.java:560)",
                "java.util.stream.ReduceOps$ReduceOp.evaluateSequential(ReduceOps.java:921)",
                "java.util.stream.AbstractPipeline.evaluate(AbstractPipeline.java:265)",
                "java.util.stream.ReferencePipeline.collect(ReferencePipeline.java:723)",
            ]
            .iter()
            .map(|f| format!("java.base/{f}"))
            .collect();
        }
        t
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
    Ok(stream_value(StreamState::Pipeline { kind: *kind, source: Source::Items(items), stages: Vec::new(), used: false }))
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
                    return Err(npe(""));
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
    b.m(BASE_STREAM, "Iterator iterator()", s_iterator);
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
    b.m(STREAM, "Optional max(Comparator)", s_max);
    b.m(STREAM, "Optional min(Comparator)", s_min);
    b.m(STREAM, "boolean noneMatch(Predicate)", t_none_match);
    b.s(STREAM, "Stream of(def[])", ss_of);
    b.m(STREAM, "Stream peek(Consumer)", s_peek);
    b.m(STREAM, "Optional reduce(BinaryOperator)", t_reduce1);
    b.m(STREAM, "def reduce(def,BinaryOperator)", t_reduce2);
    b.m(STREAM, "def reduce(def,BiFunction,BinaryOperator)", t_reduce3);
    b.m(STREAM, "Stream skip(long)", s_skip);
    b.m(STREAM, "Stream sorted()", s_sorted);
    b.m(STREAM, "Stream sorted(Comparator)", s_sorted_cmp);
    b.m(STREAM, "def[] toArray()", s_to_array);
    b.m(STREAM, "def[] toArray(IntFunction)", s_to_array_gen);
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
        b.m(c, &format!("boolean allMatch({name}Predicate)"), s_all_match);
        b.m(c, &format!("boolean anyMatch({name}Predicate)"), s_any_match);
        b.m(c, "OptionalDouble average()", s_average);
        b.m(c, "Stream boxed()", s_boxed);
        b.m(c, &format!("def collect(Supplier,Obj{name}Consumer,BiConsumer)"), s_collect3);
        b.s(c, &format!("{me} concat({me},{me})"), s_concat);
        b.m(c, "long count()", s_count);
        b.m(c, &format!("{me} distinct()"), s_distinct);
        b.m(c, &format!("{me} filter({name}Predicate)"), s_filter);
        b.m(c, &format!("{opt} findAny()"), s_find_first);
        b.m(c, &format!("{opt} findFirst()"), s_find_first);
        b.m(c, &format!("{me} flatMap({name}Function)"), s_flat_map);
        b.m(c, &format!("void forEach({consumer_sig})"), s_for_each);
        b.m(c, &format!("void forEachOrdered({consumer_sig})"), s_for_each);
        b.m(c, &format!("PrimitiveIterator.Of{name} iterator()"), s_iterator);
        b.m(c, &format!("{me} limit(long)"), s_limit);
        b.m(c, &format!("{me} map({name}UnaryOperator)"), s_map);
        b.m(c, &format!("Stream mapToObj({name}Function)"), s_map_to_obj);
        b.m(c, &format!("{opt} max()"), s_prim_max);
        b.m(c, &format!("{opt} min()"), s_prim_min);
        b.m(c, &format!("boolean noneMatch({name}Predicate)"), s_none_match);
        b.m(c, &format!("{me} peek({consumer_sig})"), s_peek);
        b.m(c, &format!("{opt} reduce({name}BinaryOperator)"), s_reduce1);
        b.m(c, &format!("{prim} reduce({prim},{name}BinaryOperator)"), s_reduce2);
        b.m(c, &format!("{me} sequential()"), s_identity);
        b.m(c, &format!("{me} skip(long)"), s_skip);
        b.m(c, &format!("{me} sorted()"), s_sorted);
        b.m(c, &format!("{prim} sum()"), s_sum);
        b.m(c, &format!("{name}SummaryStatistics summaryStatistics()"), s_summary);
        b.m(c, &format!("{arr} toArray()"), s_to_array);
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
