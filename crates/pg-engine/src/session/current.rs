//! The execution context of the statement running on this thread, for what runs deep in
//! catalog code without one: a partition key's user-defined support functions, called
//! while partitions are pruned.

use crate::exec::ExecCtx;
use std::cell::Cell;
use std::marker::PhantomData;

thread_local! {
    static CURRENT: Cell<*const ()> = const { Cell::new(std::ptr::null()) };
}

/// While it lives, `with_current` sees the context it was made from.
pub(crate) struct Entered<'a> {
    prev: *const (),
    _ctx: PhantomData<&'a ()>,
}

pub(crate) fn enter<'a>(ctx: &'a ExecCtx<'_>) -> Entered<'a> {
    let prev = CURRENT.with(|c| c.replace(ctx as *const ExecCtx<'_> as *const ()));
    Entered {
        prev,
        _ctx: PhantomData,
    }
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.prev));
    }
}

/// Run `f` with the running statement's context (None outside one).
pub fn with_current<R>(f: impl for<'x> FnOnce(&'x ExecCtx<'x>) -> R) -> Option<R> {
    let p = CURRENT.with(|c| c.get());
    if p.is_null() {
        return None;
    }
    // SAFETY: `enter` stored a context that outlives its guard, and the guard puts the
    // previous pointer back when dropped; `f` can't keep the borrow (R doesn't name 'x).
    let ctx: &ExecCtx<'_> = unsafe { &*(p as *const ExecCtx<'_>) };
    Some(f(ctx))
}
