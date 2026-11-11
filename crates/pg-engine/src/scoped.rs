//! Thread-local state set for the length of a call, put back by a guard's `Drop`: on an error
//! (`?`) and on a panic too. The server turns an engine panic into an ERROR and the session
//! goes on, so a flag set and restored by hand would stay set for the session's later
//! statements. See HANDOFF.md, "Session and thread state".
//!
//! Each guard names a point; a test can make the engine panic just after one is set
//! (`panic_at_for_test`), to check the next statements run as in a fresh session.

use std::cell::{Cell, RefCell};
use std::thread::LocalKey;

/// A thread-local `Cell` set for a scope; its previous value goes back when dropped.
#[must_use = "the value is put back when the guard is dropped"]
pub(crate) struct CellGuard<T: Copy + 'static> {
    key: &'static LocalKey<Cell<T>>,
    prev: T,
}

impl<T: Copy + 'static> Drop for CellGuard<T> {
    fn drop(&mut self) {
        self.key.set(self.prev);
    }
}

/// Set `key` to `v` until the guard drops. `point` names the place for `panic_at_for_test`.
pub(crate) fn set<T: Copy + 'static>(
    key: &'static LocalKey<Cell<T>>,
    v: T,
    point: &'static str,
) -> CellGuard<T> {
    let g = CellGuard {
        key,
        prev: key.replace(v),
    };
    checkpoint(point);
    g
}

/// A thread-local `RefCell` set for a scope; its previous value goes back when dropped.
#[must_use = "the value is put back when the guard is dropped"]
pub(crate) struct RefGuard<T: 'static> {
    key: &'static LocalKey<RefCell<T>>,
    prev: Option<T>,
}

impl<T: 'static> Drop for RefGuard<T> {
    fn drop(&mut self) {
        let prev = self.prev.take();
        // (Unwinding from a panic taken while it was borrowed: leave it.)
        let _ = self.key.try_with(|c| {
            if let (Ok(mut b), Some(p)) = (c.try_borrow_mut(), prev) {
                *b = p;
            }
        });
    }
}

/// Set `key` to `v` until the guard drops (see `set`).
pub(crate) fn set_ref<T: 'static>(
    key: &'static LocalKey<RefCell<T>>,
    v: T,
    point: &'static str,
) -> RefGuard<T> {
    let g = RefGuard {
        key,
        prev: Some(key.with(|c| c.replace(v))),
    };
    checkpoint(point);
    g
}

thread_local! {
    static PANIC_AT: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// Make the engine panic (once) on this thread when it next sets the guarded state named
/// `point` (None: don't). For tests of what a panic leaves behind.
#[doc(hidden)]
pub fn panic_at_for_test(point: Option<&'static str>) {
    PANIC_AT.set(point);
}

/// Panic here if a test asked for it (`panic_at_for_test`).
pub(crate) fn checkpoint(point: &'static str) {
    if PANIC_AT.get() == Some(point) {
        PANIC_AT.set(None);
        panic!("test panic at {point}");
    }
}

/// Clear the per-statement scratch that is set in one place and taken in another (not a
/// scope a guard can cover): at the start of each statement a session runs, so what a panic
/// left between the two never reaches a later statement.
pub(crate) fn reset_statement_scratch() {
    crate::sql::explain::reset_scratch();
    crate::sql::binder::optimize::reset_scratch();
    crate::session::ddl::event_trigger::reset_scratch();
    crate::exec::trigger::reset_scratch();
}

/// Whether every guarded flag and scratch is at rest (outside any statement): the names of
/// those that aren't. For tests.
#[doc(hidden)]
pub fn unsettled_for_test() -> Vec<&'static str> {
    let mut out = Vec::new();
    crate::sql::explain::unsettled(&mut out);
    crate::sql::binder::optimize::unsettled(&mut out);
    crate::sql::binder::unsettled(&mut out);
    crate::sql::ruleutils::unsettled(&mut out);
    crate::session::ddl::unsettled(&mut out);
    crate::session::ddl::event_trigger::unsettled(&mut out);
    crate::exec::trigger::unsettled(&mut out);
    crate::ext::postgres_fdw::modify::unsettled(&mut out);
    crate::sql::scs::unsettled(&mut out);
    crate::types::regex::unsettled(&mut out);
    out
}
