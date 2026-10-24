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

pub fn check() -> Result<()> {
    let (base, limit) = BASE.with(|b| b.get());
    if base != 0 && base.abs_diff(here()) > limit {
        return Err(
            Error::new(code::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
                .hint(depth_hint()),
        );
    }
    Ok(())
}
