//! Stack depth: Postgres's check_stack_depth, and stack that grows on demand.
//!
//! The engine runs on whatever thread embeds it (a 2 MB thread too), so deep input — long
//! operator chains, nested subqueries, recursive functions, deep JSON — must never overflow
//! the thread's stack: that aborts the host process. Two mechanisms, at the recursion points:
//!
//! - **Growth.** `guard` (and `grow`) run their closure on a fresh stack segment when the
//!   current one has less than a red zone left (`stacker`), so recursion is bounded by the
//!   depth budget below, never by the thread's stack. The fast path is one thread-local load
//!   and two compares.
//! - **The depth budget.** A statement may use `max_stack_depth` × `SCALE` bytes of stack
//!   below the session's entry, summed over the segments it grew into; beyond that it fails
//!   with Postgres's "stack depth limit exceeded" and its HINT. `SCALE` is how much bigger
//!   this engine's frames are than Postgres's for the same nesting (the build profile's), so
//!   the depths Postgres computes with the default 2MB are computed here too.

use crate::error::{Error, Result, code};
use std::cell::Cell;

/// Where the current stack segment's limits are, as addresses (the stack grows down).
#[derive(Clone, Copy)]
struct State {
    /// At or below this address `guard` switches to a new segment (the segment's end plus
    /// the red zone). `usize::MAX`: not yet known on this thread.
    grow_at: usize,
    /// Below this address the statement used more than its budget. 0: no budget (no entry
    /// marked yet).
    fail_at: usize,
    /// The current segment's base: the session's entry on the thread's own stack, or the top
    /// of a segment `grow` switched to. 0: no entry marked.
    base: usize,
    /// Stack used below the entry before this segment (0 on the thread's own stack).
    carried: usize,
    /// The current segment's lowest address (0: unknown).
    end: usize,
}

const UNKNOWN: State = State {
    grow_at: usize::MAX,
    fail_at: 0,
    base: 0,
    carried: 0,
    end: 0,
};

thread_local! {
    static ST: Cell<State> = const { Cell::new(UNKNOWN) };
    /// The red zone in force (`red`).
    static RED: Cell<usize> = const { Cell::new(RED_ZONE) };
    /// The session's max_stack_depth (kB), as the error's hint shows it.
    static SHOWN_KB: Cell<i64> = const { Cell::new(2048) };
    /// The thread's declared stack below its entry (`set_limit`), for `unused_range`.
    static DECLARED: Cell<usize> = const { Cell::new(0) };
    /// The lowest address of the thread's own stack (0: unknown).
    static OWN_END: Cell<usize> = const { Cell::new(0) };
}

/// This engine's stack per nesting level over Postgres's: the budget is max_stack_depth times
/// this. Measured on a release build against Postgres 18.6 (x86-64) at 2MB, the ratio is 14
/// (ROW nesting) to 27 (UNION arms): operator chains 16, function calls and subqueries 22,
/// NOT chains 20, PL/pgSQL calls 19. So every common shape nests at least as deep as in
/// Postgres, and errors at most about twice as deep. Debug frames are about five times
/// bigger.
const SCALE: usize = if cfg!(debug_assertions) { 160 } else { 32 };

/// Room every guarded recursion point keeps below it for the code it runs before the next
/// one (unguarded walkers, libraries).
const RED_ZONE: usize = if cfg!(debug_assertions) { 1 << 20 } else { 256 << 10 };

/// A segment `guard` grows into.
const SEGMENT: usize = if cfg!(debug_assertions) { 8 << 20 } else { 2 << 20 };

/// The largest max_stack_depth (kB): Postgres's with its usual 8MB `ulimit -s`.
pub const MAX_STACK_DEPTH_KB: i64 = 8192 - 512;

/// Note the running statement's max_stack_depth setting (kB).
pub fn set_max_depth_kb(kb: i64) {
    if SHOWN_KB.with(Cell::get) != kb {
        SHOWN_KB.with(|k| k.set(kb));
        ST.with(|s| {
            let mut st = s.get();
            st.fail_at = fail_at(st.base, st.carried);
            s.set(st);
        });
    }
}

/// The statement's budget in bytes of this engine's stack: max_stack_depth × SCALE, and the
/// allowance of the function calls running now (`FnFrame`).
fn budget() -> usize {
    (SHOWN_KB.with(Cell::get).max(100) as usize * 1024)
        .saturating_mul(SCALE)
        .saturating_add(ALLOWED.with(Cell::get))
}

/// A recursion whose depth Postgres's own frames bound before this engine's stack does
/// (JSON nesting, PL/pgSQL calls): `depth` levels of `pg_frame` bytes each (Postgres 18.6's,
/// measured against its default 2MB) fail where max_stack_depth would stop Postgres.
/// The frames of the function calls running now (`FnFrame`) count too.
#[inline]
pub fn check_emulated(depth: usize, pg_frame: usize) -> Result<()> {
    let max = SHOWN_KB.with(Cell::get).max(100) as usize * 1024;
    if depth.saturating_mul(pg_frame).saturating_add(EMULATED.with(Cell::get)) > max {
        return Err(depth_error());
    }
    check()
}

thread_local! {
    /// Postgres's stack held by the function calls running now (`FnFrame`), in bytes.
    static EMULATED: Cell<usize> = const { Cell::new(0) };
    /// This engine's stack the function calls running now add to the budget.
    static ALLOWED: Cell<usize> = const { Cell::new(0) };
}

/// The engine's stack a function call (`FnFrame::enter`) adds to the budget: a call level
/// takes about 70 kB of it in a release build (the executor's and the call's frames), where
/// Postgres's takes 0.5 to 2.2 kB, so recursive functions stop where Postgres's frames
/// would, not where this engine's stack runs out (8,422 levels of a recursive SQL function
/// take about 280 MB of stack, given back when the statement ends). Debug frames are about
/// five times bigger: there the budget alone bounds them.
const CALL_ALLOWANCE: usize = if cfg!(debug_assertions) { 0 } else { 128 << 10 };

/// One running call of a function whose recursion Postgres's frames bound (PL/pgSQL, a SQL
/// function's fmgr_sql): `pg_frame` bytes of Postgres's stack while it is alive. The calls
/// running at once add up whatever their languages, as Postgres's frames do on its stack.
pub struct FnFrame {
    pg_frame: usize,
    allowed: usize,
}

/// Postgres's stack per call of a SQL function that isn't inlined (fmgr_sql running its
/// plan): `f(n)` = `CASE WHEN n = 0 THEN 0 ELSE f(n - 1) + 1 END` runs 8,422 levels at 2MB
/// (4,211 calls; every other level is inlined), 8,423 don't.
pub const PG_SQL_FN_FRAME: usize = 498;

/// Postgres's stack per sub-select run (ExecSubPlan and the executor below it) between a
/// SQL function's call and the calls inside it: a recursive function that calls itself in a
/// scalar subquery runs 2,420 levels at 2MB (866 bytes each, its call's 498 included).
pub const PG_SUBPLAN_FRAME: usize = 368;

/// Likewise a set-returning function's call in FROM (a function scan): such a recursive
/// function runs 3,039 calls at 2MB (690 bytes each).
pub const PG_FUNCSCAN_FRAME: usize = 192;

/// And in a select list (ProjectSet): 3,438 calls (610 bytes each).
pub const PG_PROJECTSET_FRAME: usize = 112;

impl FnFrame {
    /// Enter a call: "stack depth limit exceeded" where Postgres's frames would pass
    /// max_stack_depth.
    pub fn enter(pg_frame: usize) -> Result<FnFrame> {
        let used = EMULATED.with(Cell::get) + pg_frame;
        if used > SHOWN_KB.with(Cell::get).max(100) as usize * 1024 {
            return Err(depth_error());
        }
        check()?;
        EMULATED.with(|e| e.set(used));
        allow(CALL_ALLOWANCE as isize);
        Ok(FnFrame {
            pg_frame,
            allowed: CALL_ALLOWANCE,
        })
    }

    /// Postgres's frames between two calls that check nothing themselves (a SubPlan's
    /// executor run): `pg_frame` bytes the calls running inside count.
    pub fn hold(pg_frame: usize) -> FnFrame {
        EMULATED.with(|e| e.set(e.get() + pg_frame));
        FnFrame { pg_frame, allowed: 0 }
    }
}

impl Drop for FnFrame {
    fn drop(&mut self) {
        EMULATED.with(|e| e.set(e.get().saturating_sub(self.pg_frame)));
        allow(-(self.allowed as isize));
    }
}

/// Change the budget by `bytes` (`ALLOWED`), moving the current segment's limit with it.
fn allow(bytes: isize) {
    if bytes == 0 {
        return;
    }
    ALLOWED.with(|a| a.set(a.get().saturating_add_signed(bytes)));
    ST.with(|s| {
        let mut st = s.get();
        st.fail_at = fail_at(st.base, st.carried);
        s.set(st);
    });
}

fn fail_at(base: usize, carried: usize) -> usize {
    if base == 0 {
        return 0;
    }
    base.saturating_sub(budget().saturating_sub(carried)).max(1)
}

/// Postgres's "stack depth limit exceeded", with its hint. Every place that refuses deeper
/// recursion returns this one error.
pub fn depth_error() -> Error {
    Error::new(code::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded").hint(depth_hint())
}

/// The hint of "stack depth limit exceeded".
pub fn depth_hint() -> String {
    format!(
        "Increase the configuration parameter \"max_stack_depth\" (currently {}kB), after ensuring the platform's stack depth limit is adequate.",
        SHOWN_KB.with(Cell::get)
    )
}

#[inline(always)]
fn here() -> usize {
    let x = 0u8;
    std::hint::black_box(&x) as *const u8 as usize
}

/// Make `here` the session's entry on this thread (when on the thread's own stack).
fn set_base(sp: usize) {
    ST.with(|s| {
        let mut st = s.get();
        if st.carried == 0 {
            st.base = sp;
            st.fail_at = fail_at(sp, 0);
            s.set(st);
        }
    });
}

/// Declare this thread's usable stack (bytes) below the current frame, which becomes the
/// session's entry. Threads that run sessions with big stacks call this once at their entry.
pub fn set_limit(bytes: usize) {
    DECLARED.with(|d| d.set(bytes));
    set_base(here());
}

/// Note the session's entry on this thread: the first time a session runs on it, and again
/// whenever a statement starts above the entry noted before (the host calls from a shallower
/// frame).
pub fn mark() {
    let sp = here();
    let st = ST.with(Cell::get);
    if st.base == 0 || (st.carried == 0 && sp > st.base) {
        set_base(sp);
    }
}

/// The part of this thread's declared stack (`set_limit`) below the current frame, less a
/// margin, page-aligned: (low, high) addresses no frame uses now, which an idle thread may
/// give back to the system (deep statements touched it; it reads as zeros when touched again).
pub fn unused_range() -> Option<(usize, usize)> {
    const PAGE: usize = 4096;
    const MARGIN: usize = 256 << 10;
    let st = ST.with(Cell::get);
    let limit = DECLARED.with(Cell::get);
    let sp = here();
    if st.base == 0 || st.carried != 0 || limit == 0 || sp > st.base || st.base - sp > limit {
        return None;
    }
    let lo = (st.base - limit).div_ceil(PAGE) * PAGE;
    let hi = (sp.checked_sub(MARGIN)? / PAGE) * PAGE;
    (hi > lo).then_some((lo, hi))
}

/// Postgres's check_stack_depth: the statement's stack against its budget.
#[inline]
pub fn check() -> Result<()> {
    if here() < ST.with(Cell::get).fail_at {
        return Err(depth_error());
    }
    Ok(())
}

/// Whether this runs on the thread's own stack (not on a segment `guard` grew into), with at
/// least `bytes` of it left.
pub fn on_thread_stack(bytes: usize) -> bool {
    if ST.with(Cell::get).grow_at == usize::MAX {
        with_headroom(0, || ());
    }
    let end = ST.with(Cell::get).end;
    end != 0 && end == OWN_END.with(Cell::get) && here().saturating_sub(end) >= bytes
}

/// Whether the current stack segment has less than the red zone left, for recursion that
/// can't switch stacks itself but can start over on a bigger one (the regex engine's).
#[inline]
pub fn near_end() -> bool {
    let st = ST.with(Cell::get);
    st.grow_at != usize::MAX && here() <= st.grow_at
}

/// A recursion point: `check`, then `f`, on a new stack segment if this one is nearly used up.
#[inline(always)]
pub fn guard<R>(f: impl FnOnce() -> Result<R>) -> Result<R> {
    let sp = here();
    let st = ST.with(Cell::get);
    if sp > st.grow_at && sp >= st.fail_at {
        return f();
    }
    guard_slow(sp, f)
}

#[cold]
#[inline(never)]
fn guard_slow<R>(sp: usize, f: impl FnOnce() -> Result<R>) -> Result<R> {
    if sp < ST.with(Cell::get).fail_at {
        return Err(depth_error());
    }
    grow(f)
}

/// Run `f` with at least the red zone of stack below it (growing a segment if needed), for
/// recursion that cannot fail (dropping, cloning, comparing deep values).
#[inline(always)]
pub fn grow<R>(f: impl FnOnce() -> R) -> R {
    if here() > ST.with(Cell::get).grow_at {
        return f();
    }
    with_headroom(red(), f)
}

/// The red zone: at least `RED_ZONE`, more while a long statement runs (`StatementLen`), so
/// the walks over its trees that have no guards of their own (derived `Clone`s and `Drop`s,
/// visitors) always have the stack they need below a guard.
fn red() -> usize {
    RED.with(Cell::get)
}

fn grow_at(end: usize) -> usize {
    if end == 0 { 0 } else { end + red() }
}

/// Run `f` with at least `bytes` of stack below it: on this stack when it has that much
/// left, on a new segment otherwise. For code whose recursion has no guards of its own
/// (generated decoders and `Drop`s of deep trees, C libraries), sized for its input.
pub fn with_headroom<R>(bytes: usize, f: impl FnOnce() -> R) -> R {
    let sp = here();
    let mut st = ST.with(Cell::get);
    if st.grow_at == usize::MAX {
        // First use on this thread (on its own stack): where its stack ends.
        st.end = stacker::remaining_stack().map_or(0, |left| sp.saturating_sub(left));
        OWN_END.with(|e| e.set(st.end));
        st.grow_at = grow_at(st.end);
        ST.with(|s| s.set(st));
    }
    let need = bytes.max(red());
    if st.end == 0 || sp.saturating_sub(st.end) >= need {
        return f();
    }
    on_segment(sp, st, (need + red()).max(SEGMENT), f)
}

#[cold]
#[inline(never)]
fn on_segment<R>(sp: usize, outer: State, size: usize, f: impl FnOnce() -> R) -> R {
    struct Restore(State);
    impl Drop for Restore {
        fn drop(&mut self) {
            let mut st = self.0;
            // (The budget and the red zone may have changed meanwhile.)
            st.fail_at = fail_at(st.base, st.carried);
            st.grow_at = grow_at(st.end);
            ST.with(|s| s.set(st));
        }
    }
    let carried = if outer.base == 0 {
        0
    } else {
        outer.carried + outer.base.saturating_sub(sp)
    };
    let enter = move |end: usize| {
        let top = here();
        let base = if outer.base == 0 { 0 } else { top };
        ST.with(|s| {
            s.set(State {
                grow_at: grow_at(end),
                fail_at: fail_at(base, carried),
                base,
                carried,
                end,
            })
        });
    };
    #[cfg(target_os = "linux")]
    if let Some(seg) = segments::take(size) {
        let (lo, len) = seg.usable();
        // SAFETY: the segment is this thread's, unused until it is given back below; a
        // panic is caught on it and resumed here (unwinding must not cross the switch).
        let r = unsafe {
            psm::on_stack(lo as *mut u8, len, || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _restore = Restore(outer);
                    enter(lo);
                    f()
                }))
            })
        };
        segments::give(seg);
        return r.unwrap_or_else(|p| std::panic::resume_unwind(p));
    }
    stacker::grow(size, move || {
        let _restore = Restore(outer);
        enter(here().saturating_sub(stacker::remaining_stack().unwrap_or(size / 2)));
        f()
    })
}

/// Stack segments, kept for reuse while a statement runs: recursion that keeps crossing a
/// segment's edge (a loop calling a guarded function there) switches stacks rather than
/// mapping one each time. Given back when no statement runs, they are unmapped.
#[cfg(target_os = "linux")]
mod segments {
    use std::cell::RefCell;
    use std::ffi::{c_int, c_long, c_void};

    unsafe extern "C" {
        fn mmap(a: *mut c_void, len: usize, prot: c_int, fl: c_int, fd: c_int, off: c_long)
        -> *mut c_void;
        fn munmap(a: *mut c_void, len: usize) -> c_int;
        fn mprotect(a: *mut c_void, len: usize, prot: c_int) -> c_int;
    }

    /// A guard region at the low end (a multiple of every page size).
    const GUARD: usize = 64 << 10;

    pub struct Seg {
        lo: usize,
        len: usize,
    }

    impl Seg {
        /// (lowest usable address, usable bytes)
        pub fn usable(&self) -> (usize, usize) {
            (self.lo + GUARD, self.len - GUARD)
        }
    }

    thread_local! {
        static FREE: RefCell<Vec<Seg>> = const { RefCell::new(Vec::new()) };
    }

    pub fn take(size: usize) -> Option<Seg> {
        let reuse = FREE.with(|f| {
            let mut f = f.borrow_mut();
            let i = f.iter().position(|s| s.len - GUARD >= size)?;
            Some(f.swap_remove(i))
        });
        if reuse.is_some() {
            return reuse;
        }
        let len = (size + GUARD).next_multiple_of(1 << 20);
        // MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE | MAP_STACK
        let flags = 0x02 | 0x20 | 0x4000 | 0x20000;
        // SAFETY: a fresh anonymous mapping; its lowest part made inaccessible.
        unsafe {
            let p = mmap(std::ptr::null_mut(), len, 0x1 | 0x2, flags, -1, 0);
            if p as isize == -1 {
                return None;
            }
            mprotect(p, GUARD, 0);
            Some(Seg { lo: p as usize, len })
        }
    }

    pub fn give(s: Seg) {
        // (Between statements one small segment stays: a small thread whose every statement
        // needs one maps it once.)
        let keep = super::STMT_DEPTH.with(std::cell::Cell::get) > 0
            || (s.len <= 2 * super::SEGMENT && FREE.with(|f| f.borrow().is_empty()));
        if keep {
            FREE.with(|f| f.borrow_mut().push(s));
        } else {
            unmap(s);
        }
    }

    /// Unmap the segments kept but a small one (no statement runs any more).
    pub fn release() {
        let mut kept = false;
        for s in FREE.with(|f| std::mem::take(&mut *f.borrow_mut())) {
            if !kept && s.len <= 2 * super::SEGMENT {
                kept = true;
                FREE.with(|f| f.borrow_mut().push(s));
            } else {
                unmap(s);
            }
        }
    }

    fn unmap(s: Seg) {
        drop(s);
    }

    // (Also when the thread ends with one kept.)
    impl Drop for Seg {
        fn drop(&mut self) {
            // SAFETY: the segment's own mapping, no longer in use.
            unsafe { munmap(self.lo as *mut c_void, self.len) };
        }
    }
}

thread_local! {
    /// The length of the statement text running on this thread (`StatementLen`).
    static STMT_LEN: Cell<usize> = const { Cell::new(0) };
    /// Statements running on this thread (nested ones too).
    static STMT_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Notes the running statement's text length while alive (nested statements restore the
/// outer one's): how deep its trees can be, for the red zone and `with_tree_headroom`.
pub struct StatementLen(usize, usize);

fn set_red(red: usize) {
    RED.with(|r| r.set(red));
    ST.with(|s| {
        let mut st = s.get();
        if st.grow_at != usize::MAX {
            st.grow_at = grow_at(st.end);
            s.set(st);
        }
    });
}

impl StatementLen {
    pub fn enter(len: usize) -> StatementLen {
        STMT_DEPTH.with(|d| d.set(d.get() + 1));
        let old = StatementLen(STMT_LEN.with(|l| l.replace(len)), red());
        let red = tree_headroom(len).max(RED_ZONE);
        if red != old.1 {
            set_red(red);
        }
        old
    }
}

impl Drop for StatementLen {
    fn drop(&mut self) {
        STMT_LEN.with(|l| l.set(self.0));
        if red() != self.1 {
            set_red(self.1);
        }
        let depth = STMT_DEPTH.with(|d| {
            d.set(d.get().saturating_sub(1));
            d.get()
        });
        #[cfg(target_os = "linux")]
        if depth == 0 {
            segments::release();
        }
        let _ = depth;
    }
}

/// Run `f`, a generated recursive walk over the running statement's parse tree (Debug,
/// Clone, comparisons of `pg_query` nodes), with stack for the tree's depth.
pub fn with_tree_headroom<R>(f: impl FnOnce() -> R) -> R {
    with_headroom(tree_headroom(STMT_LEN.with(Cell::get)), f)
}

/// Drop a value that may be a deep tree (a parse tree of a long statement), with stack for
/// its recursive `Drop`: `input_len` is the size of the text it came from.
pub fn drop_deep<T>(v: T, input_len: usize) {
    with_headroom(tree_headroom(input_len), move || drop(v));
}

/// The stack a generated recursive walk (decode, drop, clone, compare of a parse tree) may
/// need for a tree parsed from `input_len` bytes: a nesting level takes at least two bytes
/// of text, and the parser's own limits (its 2MB stack, its 10,000-deep grammar stack) cap
/// the depth.
pub fn tree_headroom(input_len: usize) -> usize {
    const PER_INPUT_BYTE: usize = if cfg!(debug_assertions) { 12 << 10 } else { 2 << 10 };
    const MAX: usize = if cfg!(debug_assertions) { 1 << 30 } else { 256 << 20 };
    RED_ZONE + input_len.saturating_mul(PER_INPUT_BYTE).min(MAX)
}

/// Run `f` on a stack of `size` bytes mapped below the thread's own stack, for C code that
/// sizes its depth limit from the thread's stack bounds when it runs inside them (libpg_query
/// does, capping it at 2MB; below them it keeps the limit it was given). None when no such
/// stack could be mapped.
pub fn below_thread_stack<R>(size: usize, f: impl FnOnce() -> R) -> Option<R> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::{c_int, c_long, c_void};
        unsafe extern "C" {
            fn mmap(a: *mut c_void, len: usize, prot: c_int, fl: c_int, fd: c_int, off: c_long)
            -> *mut c_void;
            fn munmap(a: *mut c_void, len: usize) -> c_int;
        }
        const RW: c_int = 0x1 | 0x2;
        // MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE | MAP_STACK | MAP_FIXED_NOREPLACE
        const FLAGS: c_int = 0x02 | 0x20 | 0x4000 | 0x20000 | 0x100000;
        if ST.with(Cell::get).grow_at == usize::MAX {
            with_headroom(0, || ());
        }
        let end = OWN_END.with(Cell::get);
        // SAFETY: a fresh anonymous mapping, unmapped below after `f` returns on it.
        unsafe {
            // (Below a thread's stack sit other mappings: try a few places further down,
            // then wherever the kernel puts it.)
            let mut p = std::ptr::null_mut();
            for k in 0..64usize {
                let Some(hint) = end.checked_sub(size + (k << 28) + (16 << 20)) else {
                    break;
                };
                let q = mmap((hint & !0xffff) as *mut c_void, size, RW, FLAGS, -1, 0);
                if q as isize != -1 {
                    p = q;
                    break;
                }
            }
            if p.is_null() {
                p = mmap(std::ptr::null_mut(), size, RW, FLAGS & !0x100000, -1, 0);
                if p as isize == -1 {
                    return None;
                }
            }
            if p as usize + size > end {
                // (Placed above it after all.)
                munmap(p, size);
                return None;
            }
            let r = psm::on_stack(p as *mut u8, size, f);
            munmap(p, size);
            Some(r)
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (size, f);
        None
    }
}
