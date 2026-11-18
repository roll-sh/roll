//! Helpers for roll search's tests: checks of how much work code does, which a loaded host
//! can't break. A debug build at load 50 can take any wall time, but its work doesn't change,
//! so these measure the calling thread's CPU time instead of a clock on the wall.

use std::time::Duration;

/// How much larger the second input of [`assert_linear`] is.
const SCALE: usize = 8;
/// The most CPU [`SCALE`] times the input may take, as a multiple of the first input's: linear
/// work takes about [`SCALE`] (n log n a little more), quadratic work about [`SCALE`] squared.
const MAX_RATIO: f64 = 24.0;

/// Asserts that `run`'s work grows linearly with its input, not quadratically: `run` on
/// `input(SCALE * small)` takes less than [`MAX_RATIO`] times the CPU of `run` on `input(small)`.
///
/// `input` isn't measured, and each size runs three times, keeping the fastest. Pick `small` so
/// that `run` takes a few milliseconds on it, and so that `run`'s fixed costs don't hide the
/// growth.
pub fn assert_linear<I>(what: &str, small: usize, input: impl Fn(usize) -> I, mut run: impl FnMut(I)) {
    let mut fastest = |size: usize| {
        (0..3)
            .map(|_| {
                let i = input(size);
                cpu_time(|| run(i)).1
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

/// `f`'s result and the CPU time this thread spent running it. Unlike wall time, it doesn't
/// grow while the thread waits for a CPU, so a bound on it holds on a loaded host. Only `f`'s
/// own thread counts: measure work that runs on the calling thread.
pub fn cpu_time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let started = thread_cpu();
    let out = f();
    (out, thread_cpu().saturating_sub(started))
}

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

#[cfg(test)]
mod tests {
    /// The check every linear-time test relies on: quadratic work fails it.
    #[test]
    #[should_panic(expected = "quadratic loop: 8x the input took")]
    fn quadratic_work_fails() {
        super::assert_linear("quadratic loop", 400, |n| n, |n| {
            for i in 0..n {
                for j in 0..n {
                    std::hint::black_box(i ^ j);
                }
            }
        });
    }
}
