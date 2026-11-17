//! Helpers for the crate's unit tests.

use std::time::Duration;

/// How much larger the second input of [`assert_linear`] is.
const SCALE: usize = 8;
/// The most CPU [`SCALE`] times the input may take, as a multiple of the first input's: linear
/// work takes about [`SCALE`] (n log n a little more), quadratic work about [`SCALE`] squared.
const MAX_RATIO: f64 = 24.0;

/// Asserts that `run`'s work grows linearly with its input, not quadratically: `run` on
/// `input(SCALE * small)` takes less than [`MAX_RATIO`] times the CPU of `run` on `input(small)`.
///
/// Measures this thread's CPU time, fastest of three runs each, rather than wall time against a
/// fixed bound: a debug build on a loaded host can take any wall time, but its work doesn't change.
/// Pick `small` so that `run` takes a few milliseconds on it.
pub(crate) fn assert_linear<I>(what: &str, small: usize, input: impl Fn(usize) -> I, mut run: impl FnMut(I)) {
    let mut fastest = |size: usize| {
        (0..3)
            .map(|_| {
                let i = input(size);
                let started = thread_cpu();
                run(i);
                thread_cpu().saturating_sub(started)
            })
            .min()
            .unwrap()
    };
    let (a, b) = (fastest(small), fastest(small * SCALE));
    let ratio = b.as_secs_f64() / a.as_secs_f64().max(1e-6);
    assert!(
        ratio < MAX_RATIO,
        "{what}: {SCALE}x the input took {ratio:.1}x the CPU ({a:?}, then {b:?}); linear work takes about {SCALE}x, quadratic {}x",
        SCALE * SCALE
    );
}

/// CPU time this thread has used: unlike wall time, it doesn't grow while the thread waits for a CPU.
#[cfg(unix)]
fn thread_cpu() -> Duration {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes only the struct it is given.
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) }, 0);
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

#[cfg(not(unix))]
fn thread_cpu() -> Duration {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed()
}
