//! Stack depth checks (Postgres's check_stack_depth): deep recursion — nested function calls,
//! subqueries, expression trees — fails with an error instead of overflowing the thread stack.

use crate::error::{Error, Result, code};
use std::cell::Cell;

thread_local! {
    /// (stack address at the session's entry, usable bytes below it)
    static BASE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    /// The session's max_stack_depth (kB), as the error's hint shows it.
    static SHOWN_KB: Cell<i64> = const { Cell::new(2048) };
}

/// Note the running statement's max_stack_depth setting (kB).
pub fn set_max_depth_kb(kb: i64) {
    SHOWN_KB.with(|k| k.set(kb));
}

/// Postgres's "stack depth limit exceeded", with its hint. Every place that refuses deeper
/// recursion returns this one error, whichever limit it hit first (the thread's stack or a
/// fixed depth): which one that is depends on frame sizes, so on the build.
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

/// Usable stack when the thread didn't declare its size (fits a 2 MB thread).
const DEFAULT_LIMIT: usize = 1536 * 1024;

#[inline(always)]
fn here() -> usize {
    let x = 0u8;
    std::hint::black_box(&x) as *const u8 as usize
}

/// Declare this thread's usable stack (bytes) below the current frame. Threads that run
/// sessions with big stacks call this once at their entry.
pub fn set_limit(bytes: usize) {
    BASE.with(|b| b.set((here(), bytes)));
}

/// Remember where the stack starts the first time a session runs on this thread.
pub fn mark() {
    BASE.with(|b| {
        if b.get().0 == 0 {
            b.set((here(), DEFAULT_LIMIT));
        }
    });
}

/// The part of this thread's declared stack (`set_limit`) below the current frame, less a
/// margin, page-aligned: (low, high) addresses no frame uses now, which an idle thread may
/// give back to the system (deep statements touched it; it reads as zeros when touched again).
pub fn unused_range() -> Option<(usize, usize)> {
    const PAGE: usize = 4096;
    const MARGIN: usize = 256 << 10;
    let (base, limit) = BASE.with(|b| b.get());
    let sp = here();
    if base == 0 || sp > base || base - sp > limit {
        return None;
    }
    let lo = (base - limit).div_ceil(PAGE) * PAGE;
    let hi = (sp.checked_sub(MARGIN)? / PAGE) * PAGE;
    (hi > lo).then_some((lo, hi))
}

pub fn check() -> Result<()> {
    let (base, limit) = BASE.with(|b| b.get());
    if base != 0 && base.abs_diff(here()) > limit {
        return Err(depth_error());
    }
    Ok(())
}
