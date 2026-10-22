//! Stack guard for recursion over untrusted nesting.
//!
//! Request bodies may nest up to 1000 levels (Jackson's limit, which Elasticsearch accepts in
//! `_meta`, settings, intervals sources and geometry coordinates), and the code that walks them —
//! settings flattening, the Query DSL, mapping, document and geometry parsing, the writers —
//! recurses once per level. Tokio workers run on 2 MiB stacks and debug-build frames are large, so
//! a deep body could overflow the stack, and a stack overflow aborts the process: every tenant on
//! the node, not just the request.
//!
//! Recursive walkers wrap each level in [`maybe_grow`]. While more than [`RED_ZONE`] bytes of the
//! current thread's stack are left it just calls the closure (one thread-local load and a
//! compare); below that the walk continues on a fresh thread with a [`GROWN`] stack while the
//! caller blocks, which is what a segmented stack would do. Only pathological nesting ever pays
//! for the thread. Where the platform can't report the stack bounds the closure is called
//! directly.

use std::cell::Cell;

/// Below this much free stack, the next level runs on a new thread. Generous because debug
/// frames of the big parsers run to tens of KiB and a level may span several of them.
pub const RED_ZONE: usize = 256 * 1024;

/// Stack size of the continuation threads (reserved virtual memory, committed as touched).
pub const GROWN: usize = 32 * 1024 * 1024;

thread_local! {
    /// Lowest usable address of this thread's stack; 0 = not looked up yet, 1 = unknown.
    static LIMIT: Cell<usize> = const { Cell::new(0) };
}

/// Bytes of stack left on the current thread, when the platform says where the stack ends.
#[inline]
pub fn remaining() -> Option<usize> {
    let limit = LIMIT.with(|l| {
        let v = l.get();
        if v != 0 {
            return v;
        }
        let v = stack_low().unwrap_or(1);
        l.set(v);
        v
    });
    if limit == 1 {
        return None;
    }
    let marker = 0u8;
    let sp = std::hint::black_box(&marker) as *const u8 as usize;
    Some(sp.saturating_sub(limit))
}

/// Runs `f`, on a new thread with a [`GROWN`] stack if less than [`RED_ZONE`] is left here.
#[inline]
pub fn maybe_grow<R, F: FnOnce() -> R>(f: F) -> R {
    match remaining() {
        Some(left) if left < RED_ZONE => grow(f),
        _ => f(),
    }
}

/// Runs `f` on a continuation thread and waits for it.
#[cold]
#[inline(never)]
fn grow<R, F: FnOnce() -> R>(f: F) -> R {
    struct AssertSend<T>(T);
    // SAFETY: the calling thread does nothing but wait for the scoped thread, so whatever `f`
    // borrows or owns is used by exactly one thread at a time, with the spawn and the join as
    // the hand-over points (as if the stack had been switched in place). The walkers this guards
    // are synchronous parsers and printers that hold no thread-affine state (lock guards,
    // thread-local references) across a level.
    unsafe impl<T> Send for AssertSend<T> {}
    let job = AssertSend(f);
    let out = std::thread::scope(|s| {
        let handle = std::thread::Builder::new()
            .name("search-deep".into())
            .stack_size(GROWN)
            .spawn_scoped(s, move || {
                let job = job;
                AssertSend((job.0)())
            })
            .expect("spawning a thread for deeply nested input");
        match handle.join() {
            Ok(r) => r,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    });
    out.0
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn stack_low() -> Option<usize> {
    // SAFETY: plain libc queries on the calling thread with a zeroed, then initialised and
    // destroyed, attribute object.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let ok = libc::pthread_attr_getstack(&attr, &mut addr, &mut size) == 0;
        libc::pthread_attr_destroy(&mut attr);
        (ok && !addr.is_null() && size > 0).then_some(addr as usize)
    }
}

#[cfg(target_os = "macos")]
fn stack_low() -> Option<usize> {
    // SAFETY: plain libc queries about the calling thread.
    unsafe {
        let me = libc::pthread_self();
        let top = libc::pthread_get_stackaddr_np(me) as usize;
        let size = libc::pthread_get_stacksize_np(me);
        (top > size && size > 0).then(|| top - size)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn stack_low() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth(n: usize, pad: &mut [u8; 4096]) -> usize {
        // A big frame per level, like the debug-build parsers.
        let mut local = [0u8; 4096];
        local[n % 4096] = pad[n % 4096].wrapping_add(1);
        if n == 0 {
            return std::hint::black_box(&local)[0] as usize;
        }
        maybe_grow(|| depth(n - 1, &mut local)) + 1
    }

    #[test]
    fn deep_recursion_survives_small_stacks() {
        // 4 KiB+ per level, 10 000 levels: ~40 MiB of frames from a 512 KiB thread.
        let n = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| depth(10_000, &mut [0u8; 4096]))
            .unwrap()
            .join()
            .unwrap();
        assert!(n >= 10_000);
    }

    #[test]
    fn reports_the_stack() {
        let left = std::thread::Builder::new().stack_size(1 << 20).spawn(remaining).unwrap().join().unwrap();
        // glibc may hand the thread a cached, larger stack; at least the requested size is left.
        if let Some(left) = left {
            assert!(left > 512 * 1024 && left <= GROWN, "{left}");
        }
    }

    #[test]
    fn panics_cross_the_continuation() {
        let r = std::panic::catch_unwind(|| grow(|| panic!("boom")));
        assert!(r.is_err());
    }
}
