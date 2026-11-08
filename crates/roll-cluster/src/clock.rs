//! The node's own clock for lease deadlines.
//!
//! Monotonic, and on Linux it also counts time the machine was suspended (`CLOCK_BOOTTIME`): a
//! node that wakes from a suspend sees its leases as expired, instead of believing they still
//! have the time left that they had when it went to sleep (`CLOCK_MONOTONIC`, which
//! `std::time::Instant` uses, stops during a suspend).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Time since an arbitrary fixed origin. Only differences mean anything.
pub(crate) fn now() -> Duration {
    static BOOTTIME: OnceLock<bool> = OnceLock::new();
    if *BOOTTIME.get_or_init(|| boottime().is_some())
        && let Some(t) = boottime()
    {
        return t;
    }
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn boottime() -> Option<Duration> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime only writes the timespec we hand it.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    (rc == 0 && ts.tv_sec >= 0).then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn boottime() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn is_monotonic() {
        let a = super::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = super::now();
        assert!(b >= a + std::time::Duration::from_millis(5), "{a:?} {b:?}");
    }
}
