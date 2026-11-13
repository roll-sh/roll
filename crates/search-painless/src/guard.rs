//! Guards that keep one script from taking the whole node down.
//!
//! Elasticsearch runs scripts on a JVM that turns runaway scripts into exceptions: a deep
//! recursion is a `StackOverflowError`, a huge array an `OutOfMemoryError`, both converted to a
//! `script_exception`. A Rust process aborts on both instead, so everything a script can make
//! arbitrarily deep or large goes through this module:
//!
//! - **Native stack.** Executions and compilations open a [`StackScope`] that fixes the lowest
//!   stack address recursive code may reach. Recursive algorithms over script data (`toString`,
//!   `hashCode`, `equals`, comparisons, JSON conversion) and the compiler's tree walks test
//!   [`stack_exhausted`] at each level. Code that cannot return an error records the overflow
//!   with [`note_overflow`] and returns a placeholder; the interpreter turns the record into a
//!   `stack_overflow_error` after the library call (see [`take_pending`]).
//! - **Dropping.** Values nest without bound (`l = [l]` a million times), and dropping a nested
//!   value recursively would overflow the stack. The containers hand their contents to
//!   [`drop_nested`], which drops recursively up to a fixed depth and queues deeper levels on a
//!   thread-local work list drained by the outermost drop: constant stack, no allocation unless
//!   the nesting is actually deep.
//! - **Object size.** A script-controlled size (`new int[n]`, a doubling string, `addAll` of a
//!   list onto itself) is checked with [`check_bytes`] against a per-object cap before memory is
//!   allocated, and fails like the JVM: an uncatchable `OutOfMemoryError`.

use std::cell::{Cell, RefCell};

use crate::types::cls;
use crate::value::{JIndexMap, Value};
use crate::vm::{Throw, Thrown};

/// Native stack one execution may use below its entry frame. Hosts run scripts on threads
/// with at least 2 MiB of stack (Rust's default), which leaves room for the host's own frames.
pub const EXECUTION_STACK: usize = 1024 * 1024;

/// Stack of the thread a compilation runs on, and how much of it parsing and analysis may use.
/// They recurse once per nesting level of the source and refuse deeper sources; the later
/// passes (code generation, the fast-path planner, dropping the trees) walk the same depth with
/// the rest of the stack. Elasticsearch fails somewhere between 300 and 1,000 levels of
/// parentheses; this allows several thousand.
pub const COMPILE_THREAD_STACK: usize = 256 * 1024 * 1024;
pub const COMPILE_STACK: usize = 64 * 1024 * 1024;

/// Default for [`crate::Settings::max_object_bytes`].
pub const DEFAULT_MAX_OBJECT_BYTES: usize = 256 * 1024 * 1024;

thread_local! {
    /// Lowest stack address recursive code may reach; 0 outside executions and compilations.
    static STACK_LIMIT: Cell<usize> = const { Cell::new(0) };
    /// A failure recorded by code that could not return an error.
    static PENDING: Cell<Pending> = const { Cell::new(Pending::None) };
    /// Largest single object (array, string, collection) a script may build, in bytes.
    static MAX_OBJECT: Cell<usize> = const { Cell::new(DEFAULT_MAX_OBJECT_BYTES) };
    /// Recursion depth of [`drop_nested`].
    static DROP_DEPTH: Cell<u32> = const { Cell::new(0) };
    static DEFERRED: RefCell<Vec<Deferred>> = const { RefCell::new(Vec::new()) };
    static PENDING_THROWN: RefCell<Option<Thrown>> = const { RefCell::new(None) };
}

/// Where Java detects a `BigInteger` range overflow (picks the JDK frames of the exception).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BigSite {
    /// `checkRange` in a constructor.
    Range,
    /// `checkRange` of `shiftLeft`'s result.
    ShiftLeft,
    /// `checkRange` of `shiftRight(Integer.MIN_VALUE)`'s result.
    ShiftRight,
    /// `pow`: the power of two does not shift by an `int`.
    PowShift,
    /// `pow`: the size estimate.
    Pow,
    /// `pow` of ±2^k: its final `shiftLeft`.
    PowShiftLeft,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    None,
    Overflow,
    OutOfMemory,
    /// `BigInteger would overflow supported range`, from arithmetic that returns plain values.
    BigOverflow(BigSite),
    /// Any other exception, held in `PENDING_THROWN`.
    Thrown,
}

/// The current stack address (approximately: the address of a local).
#[inline(always)]
pub fn sp() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

/// Bounds the native stack of the code running while it is alive (an execution, a
/// compilation). Nested scopes only ever tighten the limit.
pub struct StackScope {
    prev: usize,
    prev_max: usize,
}

impl StackScope {
    pub fn enter(budget: usize, max_object: usize) -> StackScope {
        let prev = STACK_LIMIT.get();
        let limit = sp().saturating_sub(budget).max(1);
        if prev == 0 || limit > prev {
            STACK_LIMIT.set(limit);
        }
        let prev_max = MAX_OBJECT.replace(max_object);
        StackScope { prev, prev_max }
    }
}

/// Bounds the stack of a public entry point (`Value::to_java_string` called by a host) when
/// no execution has: recursive code then stops [`EXECUTION_STACK`] below it.
pub struct OuterScope {
    _inner: Option<StackScope>,
}

impl OuterScope {
    #[inline]
    pub fn enter() -> OuterScope {
        if STACK_LIMIT.get() != 0 {
            return OuterScope { _inner: None };
        }
        OuterScope { _inner: Some(StackScope::enter(EXECUTION_STACK, MAX_OBJECT.get())) }
    }
}

impl Drop for StackScope {
    fn drop(&mut self) {
        STACK_LIMIT.set(self.prev);
        MAX_OBJECT.set(self.prev_max);
    }
}

/// Whether the current frame is below the limit of the enclosing [`StackScope`].
#[inline(always)]
pub fn stack_exhausted() -> bool {
    let limit = STACK_LIMIT.get();
    limit != 0 && sp() < limit
}

/// Records a stack overflow inside code that cannot return an error.
#[cold]
pub fn note_overflow() {
    if PENDING.get() == Pending::None {
        PENDING.set(Pending::Overflow);
    }
}

/// Records an object grown past [`max_object_bytes`] inside code that cannot return an error
/// (`toString` of a value whose text would be larger, …).
#[cold]
pub fn note_out_of_memory() {
    if PENDING.get() == Pending::None {
        PENDING.set(Pending::OutOfMemory);
    }
}

/// Records a `BigInteger` result outside Java's supported range (2^31 - 1 bits) inside
/// arithmetic that cannot return an error; it surfaces as Java's `ArithmeticException` after the
/// library call.
#[cold]
pub fn note_big_overflow(site: BigSite) {
    if PENDING.get() == Pending::None {
        PENDING.set(Pending::BigOverflow(site));
    }
}

/// Takes a recorded `BigInteger` overflow (only that kind of pending failure).
pub fn take_big_overflow() -> Option<BigSite> {
    match PENDING.get() {
        Pending::BigOverflow(site) => {
            PENDING.set(Pending::None);
            Some(site)
        }
        _ => None,
    }
}

/// Records an exception raised inside code that cannot return one (a `toString` building text
/// the JDK would fail to build); it surfaces after the library call.
#[cold]
pub fn note_thrown(t: Thrown) {
    if PENDING.get() == Pending::None {
        PENDING.set(Pending::Thrown);
        PENDING_THROWN.with(|p| *p.borrow_mut() = Some(t));
    }
}

/// For recursions over script data that cannot return an error: whether to stop here (the
/// overflow is recorded and surfaces as a `stack_overflow_error` after the library call).
#[inline(always)]
pub fn too_deep() -> bool {
    if stack_exhausted() {
        note_overflow();
        return true;
    }
    false
}

/// For text built by code that cannot return an error: whether `len` bytes is too much (the
/// failure is recorded and surfaces as an `out_of_memory_error` after the library call).
#[inline(always)]
pub fn too_long(len: usize) -> bool {
    if len > MAX_OBJECT.get() {
        note_out_of_memory();
        return true;
    }
    false
}

/// Clears the recorded failure and returns it as the error to throw.
#[inline(always)]
pub fn take_pending() -> Option<Thrown> {
    match PENDING.get() {
        Pending::None => None,
        p => {
            PENDING.set(Pending::None);
            Some(match p {
                Pending::OutOfMemory => out_of_memory("Java heap space"),
                Pending::BigOverflow(site) => crate::bignum::overflow_error(site, &[]),
                Pending::Thrown => PENDING_THROWN.with(|p| p.borrow_mut().take()).unwrap_or_else(|| out_of_memory("Java heap space")),
                _ => stack_overflow(),
            })
        }
    }
}

/// The compile error of a source nested too deeply for the compiler's stack (Elasticsearch:
/// a `StackOverflowError` while parsing, reported without a location).
pub fn compile_overflow() -> crate::error::CompileError {
    crate::error::CompileError::unlocated("stack_overflow_error", "")
}

/// [`compile_overflow`] when the compiler is out of stack.
#[inline(always)]
pub fn compile_stack() -> Result<(), crate::error::CompileError> {
    if stack_exhausted() {
        return Err(compile_overflow());
    }
    Ok(())
}

/// `java.lang.StackOverflowError` (uncatchable, reported as `stack_overflow_error`).
pub fn stack_overflow() -> Thrown {
    let mut t = Throw::new(cls::THROWABLE, None);
    t.catchable = false;
    t.es_type = Some("stack_overflow_error");
    t
}

/// `java.lang.OutOfMemoryError` with the JVM's message (uncatchable, reported as
/// `out_of_memory_error`).
pub fn out_of_memory(message: &str) -> Thrown {
    let mut t = Throw::new(cls::THROWABLE, Some(message.to_string()));
    t.catchable = false;
    t.es_type = Some("out_of_memory_error");
    t
}

/// The JVM refuses arrays longer than this outright.
pub const MAX_ARRAY_LENGTH: u64 = i32::MAX as u64 - 2;

/// Checks that an object of `bytes` bytes may be created.
#[inline]
pub fn check_bytes(bytes: u64) -> Result<(), Thrown> {
    if bytes > MAX_OBJECT.get() as u64 {
        return Err(out_of_memory("Java heap space"));
    }
    Ok(())
}

/// Checks a new array of `len` elements of `elem_bytes` each (Java's array header included).
pub fn check_array(len: u64, elem_bytes: u64) -> Result<(), Thrown> {
    if len > MAX_ARRAY_LENGTH {
        return Err(out_of_memory("Requested array size exceeds VM limit"));
    }
    check_bytes(16u64.saturating_add(len.saturating_mul(elem_bytes)))
}

/// Checks that a string may grow to `len` UTF-16 units (`String`, `StringBuilder`).
#[inline]
pub fn check_string(len: usize) -> Result<(), Thrown> {
    if len as u64 > MAX_ARRAY_LENGTH {
        return Err(out_of_memory("Requested array size exceeds VM limit"));
    }
    // Java strings are Latin-1 or UTF-16; charge one byte per unit like a Latin-1 string
    check_bytes(len as u64)
}

/// Checks that a collection may hold `len` elements (a reference each, as a Java collection's
/// backing array).
#[inline]
pub fn check_elements(len: usize) -> Result<(), Thrown> {
    if len < 4096 {
        return Ok(());
    }
    check_array(len as u64, std::mem::size_of::<Value>() as u64)
}

/// The capacity of a `java.lang.StringBuilder` (JDK 27, compact strings) being filled by library
/// code, so text the JDK builds in one can fail where and how it does: an `int` length overflow is
/// `OutOfMemoryError: Required array length <old> + <growth> is too large`, and each buffer it
/// would allocate is checked against [`max_object_bytes`] before the text itself is built.
pub struct JavaSb {
    /// `value.length` in bytes.
    bytes: i32,
    /// 0 Latin-1, 1 UTF-16.
    coder: u32,
    count: i32,
}

impl Default for JavaSb {
    fn default() -> JavaSb {
        JavaSb { bytes: 16, coder: 0, count: 0 }
    }
}

const SOFT_MAX_ARRAY_LENGTH: i32 = i32::MAX - 8;

/// `ArraysSupport.newLength`.
fn new_length(old: i32, min_growth: i32, pref_growth: i32) -> Result<i32, Thrown> {
    let pref = old.wrapping_add(min_growth.max(pref_growth));
    if 0 < pref && pref <= SOFT_MAX_ARRAY_LENGTH {
        return Ok(pref);
    }
    let min = old.wrapping_add(min_growth);
    if min < 0 {
        let mut t = out_of_memory(&format!("Required array length {old} + {min_growth} is too large"));
        t.frames = vec!["java.base/jdk.internal.util.ArraysSupport.hugeLength(ArraysSupport.java:914)".into(), "java.base/jdk.internal.util.ArraysSupport.newLength(ArraysSupport.java:907)".into()];
        return Err(t);
    }
    Ok(min.max(SOFT_MAX_ARRAY_LENGTH))
}

impl JavaSb {
    /// `ensureCapacityNewCoder(…, limit, newCoder)`, with Java's `int` arithmetic.
    fn ensure(&mut self, limit: i32, coder: u32) -> Result<(), Thrown> {
        let needed = (limit as i64) << coder;
        if coder == self.coder && needed <= self.bytes as i64 && needed >= 0 {
            return Ok(());
        }
        let old_cap = self.bytes >> self.coder;
        let cap = if limit.wrapping_sub(old_cap) <= 0 {
            old_cap
        } else {
            let len = new_length(self.bytes, limit.wrapping_shl(coder).wrapping_sub(self.bytes), self.bytes.wrapping_add(2 << coder)).map_err(|mut t| {
                t.frames.push("java.base/java.lang.AbstractStringBuilder.newCapacity(AbstractStringBuilder.java:344)".into());
                t.frames.push("java.base/java.lang.AbstractStringBuilder.ensureCapacityNewCoder(AbstractStringBuilder.java:276)".into());
                t
            })?;
            if len == i32::MAX {
                return Err(out_of_memory("Required length exceeds implementation limit"));
            }
            len >> coder
        };
        let bytes = (cap as u64) << coder;
        check_bytes(16 + bytes)?;
        self.bytes = bytes as i32;
        self.coder = coder;
        Ok(())
    }

    fn coder_of(max_char: u32) -> u32 {
        (max_char > 0xFF) as u32
    }

    /// `n` chars appended one or a few at a time (`append(char)`, short strings).
    pub fn append(&mut self, n: usize, max_char: u32) -> Result<(), Thrown> {
        let coder = self.coder | JavaSb::coder_of(max_char);
        let target = self.count as i64 + n as i64;
        if n > 0 && coder != self.coder {
            self.ensure(self.count, coder)?;
        }
        while (self.count as i64) < target {
            let room = (self.bytes >> self.coder) as i64 - self.count as i64;
            if room >= target - self.count as i64 {
                self.count = target as i32;
                break;
            }
            self.count += room as i32;
            self.ensure(self.count.wrapping_add(1), coder)?;
        }
        Ok(())
    }

    /// `repeat(char, count)`: one capacity request for all of them.
    pub fn repeat(&mut self, count: i32, c: u32) -> Result<(), Thrown> {
        if count < 0 {
            return Err(crate::vm::iae(format!("count is negative: {count}")));
        }
        if count == 0 {
            return Ok(());
        }
        let limit = self.count.wrapping_add(count);
        self.ensure(limit, self.coder | JavaSb::coder_of(c)).map_err(|mut t| {
            if !t.frames.is_empty() {
                t.frames.push("java.base/java.lang.AbstractStringBuilder.repeat(AbstractStringBuilder.java:2056)".into());
                t.frames.push("java.base/java.lang.AbstractStringBuilder.repeat(AbstractStringBuilder.java:2100)".into());
                t.frames.push("java.base/java.lang.StringBuilder.repeat(StringBuilder.java:456)".into());
            }
            t
        })?;
        self.count = limit;
        Ok(())
    }
}

/// The largest object a script may build in the current execution, in bytes.
pub fn max_object_bytes() -> usize {
    MAX_OBJECT.get()
}

// ------------------------------------------------------------------------------------ drops

/// Contents of a container being dropped (held only to be dropped).
#[allow(dead_code)]
pub enum Deferred {
    Values(Vec<Value>),
    Map(JIndexMap),
    One(Value),
}

/// Levels dropped recursively before deeper ones are queued.
const MAX_DROP_DEPTH: u32 = 128;

/// Whether dropping `v` may recurse (it holds a container).
#[inline(always)]
pub fn nests(v: &Value) -> bool {
    matches!(v, Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Obj(_))
}

/// Drops a container's contents with bounded native recursion (see the module docs).
pub fn drop_nested(d: Deferred) {
    let depth = DROP_DEPTH.get();
    if depth >= MAX_DROP_DEPTH {
        // queued for the outermost drop; during thread teardown the queue may be gone already,
        // and then the contents drop here
        let rest = DEFERRED.try_with(|q| q.borrow_mut().push(d));
        if let Err(_gone) = rest {
            // `d` was moved into the closure and dropped with it
        }
        return;
    }
    DROP_DEPTH.set(depth + 1);
    drop(d);
    if depth == 0 {
        loop {
            let next = DEFERRED.try_with(|q| q.borrow_mut().pop()).ok().flatten();
            match next {
                Some(n) => drop(n),
                None => break,
            }
        }
    }
    DROP_DEPTH.set(depth);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::JList;
    use std::rc::Rc;

    #[test]
    fn deep_lists_drop_without_recursion() {
        // on a small stack: a recursive drop of a million levels would overflow it
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let mut v = JList::array_list(Vec::new());
                for _ in 0..1_000_000 {
                    v = JList::array_list(vec![v]);
                }
                drop(v);
                let mut m = Value::Map(Rc::new(crate::value::JMap::hash_map()));
                for _ in 0..200_000 {
                    let n = crate::value::JMap::hash_map();
                    n.insert_raw(Value::str("a"), m);
                    m = Value::Map(Rc::new(n));
                }
                drop(m);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn stack_scope_limits_and_restores() {
        assert!(!stack_exhausted());
        {
            let _s = StackScope::enter(64 * 1024, 1 << 20);
            assert!(!stack_exhausted());
            fn deep(n: u32) -> u32 {
                if stack_exhausted() {
                    return n;
                }
                let pad = [n as u8; 512];
                std::hint::black_box(&pad);
                deep(n + 1) + std::hint::black_box(0)
            }
            let levels = deep(0);
            assert!(levels > 10 && levels < 1000, "{levels}");
            assert_eq!(max_object_bytes(), 1 << 20);
            assert!(check_bytes(2 << 20).is_err());
        }
        assert!(!stack_exhausted());
        assert_eq!(max_object_bytes(), DEFAULT_MAX_OBJECT_BYTES);
    }

    #[test]
    fn array_limits_follow_the_jvm() {
        let e = check_array(MAX_ARRAY_LENGTH + 1, 1).unwrap_err();
        assert_eq!(e.message.as_deref(), Some("Requested array size exceeds VM limit"));
        let e = check_array(1 << 30, 8).unwrap_err();
        assert_eq!(e.message.as_deref(), Some("Java heap space"));
        assert!(check_array(1 << 20, 8).is_ok());
    }
}
