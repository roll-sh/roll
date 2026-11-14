//! Process-wide memory tuning for nodes that host many mostly-idle clusters.
//!
//! An idle node should cost little, and an unloaded cluster's memory should leave the process.
//! Two allocator/kernel defaults work against that:
//!
//! - **mimalloc's purge delay** (10 ms, times 10 for arenas) keeps freed pages committed for a
//!   while; [`tune_memory`] sets it to 0, so pages go back at once (workers also run
//!   `mi_collect` when they park: [`on_thread_park`]). A delay of 0 makes a busy process fault
//!   every page it frees and reuses back in (bulk indexing: about 8% of its CPU in page faults
//!   and page clearing), so [`start_purge_governor`] raises it to [`BUSY_PURGE_DELAY_MS`] while
//!   the process uses CPU and, once it is idle, drops it to 0 again and has every runtime
//!   worker collect its heap, leaving an idle process as small as before. Once the process has
//!   stayed idle that long, the governor parks until [`note_activity`] (every request) wakes
//!   it, so an idle node makes no system calls at all.
//! - **Transparent huge pages** (`/sys/kernel/mm/transparent_hugepage/enabled` = `always`, the
//!   default on many distributions) back every touched 2 MiB stretch of mimalloc's arena with a
//!   whole huge page. A heap that is mostly small, sparse allocations (route tables, analysis
//!   tables, per-cluster state) then shows several times its live size in RSS: measured on
//!   search-dev, 36 MiB of `AnonHugePages` at start for about 10 MiB of live heap. Disabling
//!   THP for the process (`PR_SET_THP_DISABLE`) trades a little TLB reach for an RSS that
//!   follows the live heap; segment data is file-backed (memory maps of the cache) and is not
//!   affected.
//!
//! Call [`tune_memory`] first thing in `main`, before the runtime starts.

/// `mi_option_purge_delay` (libmimalloc-sys doesn't export the constant).
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

/// What [`tune_memory`] managed to apply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tuned {
    /// Transparent huge pages are disabled for the process.
    pub thp_disabled: bool,
}

/// Returns freed memory to the OS at once and disables transparent huge pages for the process
/// (Linux). Harmless when mimalloc isn't the global allocator.
pub fn tune_memory() -> Tuned {
    // `ROLL_MI_PURGE_DELAY_MS` overrides the delay (benchmarks: allocation-heavy requests such
    // as 1,000-hit pages refault every freed page with 0).
    let delay = std::env::var("ROLL_MI_PURGE_DELAY_MS").ok().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, delay as _) };
    Tuned { thp_disabled: disable_thp() }
}

#[cfg(target_os = "linux")]
fn disable_thp() -> bool {
    // SAFETY: PR_SET_THP_DISABLE takes integer arguments only and touches no memory of ours.
    unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) == 0 }
}

#[cfg(not(target_os = "linux"))]
fn disable_thp() -> bool {
    false
}

/// For `tokio::runtime::Builder::on_thread_park`: hands a parking worker's freed pages back.
pub fn on_thread_park() {
    unsafe { libmimalloc_sys::mi_collect(false) };
}

/// The purge delay while the process is busy: freed pages stay committed this long, so
/// allocation-heavy work (bulk indexing, large result pages) reuses them instead of faulting
/// them in again (8-client geonames bulk: +7% docs/s, -8% CPU per document).
pub const BUSY_PURGE_DELAY_MS: i64 = 100;
/// Governor checks are this far apart.
const GOVERNOR_TICK: std::time::Duration = std::time::Duration::from_millis(250);
/// Process CPU per tick above which the process counts as busy (10% of one core).
const BUSY_CPU_PER_TICK: std::time::Duration = std::time::Duration::from_millis(25);
/// Idle ticks before the delay drops back to 0 and memory is collected: past the arena purge
/// delay (10 × the busy delay) and past the search pool's idle exit (2 s), so every page freed
/// while busy can go back and exited workers' heaps are reclaimed.
const IDLE_TICKS: u32 = 10;
/// A second collection, once tokio's blocking threads (10 s keep-alive) have exited.
const LATE_IDLE_TICKS: u32 = 48;

/// The governor thread, while it is parked (an idle process): [`note_activity`] wakes it.
static PARKED_GOVERNOR: std::sync::Mutex<Option<std::thread::Thread>> = std::sync::Mutex::new(None);
/// Whether [`PARKED_GOVERNOR`] holds a parked thread (one relaxed load per request otherwise).
static GOVERNOR_PARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tells the purge governor the process has work again (a request arrived). Wakes it if it
/// parked after a long idle stretch; costs one atomic load when it is running.
pub fn note_activity() {
    use std::sync::atomic::Ordering;
    if GOVERNOR_PARKED.load(Ordering::Relaxed)
        && GOVERNOR_PARKED.swap(false, Ordering::AcqRel)
        && let Some(t) = PARKED_GOVERNOR.lock().unwrap_or_else(|e| e.into_inner()).take()
    {
        t.unpark();
    }
}

/// Parks the calling governor until [`note_activity`].
fn park_governor() {
    use std::sync::atomic::Ordering;
    *PARKED_GOVERNOR.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current());
    GOVERNOR_PARKED.store(true, Ordering::Release);
    // `park` may return spuriously, and an unpark that came first leaves a token: either way
    // the flag tells whether activity was noted.
    while GOVERNOR_PARKED.load(Ordering::Acquire) {
        std::thread::park();
    }
}

/// User plus system CPU of the whole process.
#[cfg(unix)]
fn process_cpu() -> std::time::Duration {
    // SAFETY: getrusage writes only the struct it is given.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) } != 0 {
        return std::time::Duration::ZERO;
    }
    let tv = |t: libc::timeval| std::time::Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}

#[cfg(not(unix))]
fn process_cpu() -> std::time::Duration {
    std::time::Duration::ZERO
}

/// Starts the thread that switches mimalloc's purge delay between [`BUSY_PURGE_DELAY_MS`]
/// (while the process uses CPU) and 0 (idle), on top of [`tune_memory`]. Going idle, it
/// collects (forcing the arenas' pending purges) and wakes `workers` worker threads of `rt`,
/// each of which collects its own heap when it parks again ([`on_thread_park`]). Does nothing
/// when `ROLL_MI_PURGE_DELAY_MS` fixes the delay.
pub fn start_purge_governor(rt: tokio::runtime::Handle, workers: usize) {
    if std::env::var_os("ROLL_MI_PURGE_DELAY_MS").is_some() {
        return;
    }
    let _ = std::thread::Builder::new().name("roll-mem-governor".into()).spawn(move || {
        let mut busy = false;
        let mut idle_ticks = 0u32;
        let mut last = process_cpu();
        loop {
            std::thread::sleep(GOVERNOR_TICK);
            let now = process_cpu();
            let used = now.saturating_sub(last);
            last = now;
            if used >= BUSY_CPU_PER_TICK {
                idle_ticks = 0;
                if !busy {
                    busy = true;
                    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, BUSY_PURGE_DELAY_MS as _) };
                }
                continue;
            }
            if !busy && idle_ticks >= LATE_IDLE_TICKS {
                // Idle and collected: nothing to do until a request comes.
                park_governor();
                idle_ticks = 0;
                last = process_cpu();
                continue;
            }
            idle_ticks += 1;
            if idle_ticks == IDLE_TICKS {
                busy = false;
                unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
            } else if idle_ticks != LATE_IDLE_TICKS {
                continue;
            }
            // A forced collection reclaims the heaps of exited threads and purges every
            // arena; parked workers hold their own freed pages until they run, so each gets
            // a task that collects its heap.
            unsafe { libmimalloc_sys::mi_collect(true) };
            for _ in 0..workers * 2 {
                rt.spawn(async {
                    unsafe { libmimalloc_sys::mi_collect(true) };
                    tokio::task::yield_now().await
                });
            }
            // The waking itself counts as CPU; don't read it as new work.
            std::thread::sleep(GOVERNOR_TICK);
            last = process_cpu();
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_parked_governor_wakes_on_activity() {
        use std::sync::atomic::Ordering;
        let woke = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let w = woke.clone();
        let t = std::thread::spawn(move || {
            super::park_governor();
            w.store(true, Ordering::Release);
        });
        while !super::GOVERNOR_PARKED.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        // (Other tests in this process serve requests, which note activity too: whether it
        // stays parked meanwhile isn't something this test can see.)
        super::note_activity();
        t.join().unwrap();
        assert!(woke.load(Ordering::Acquire));
        // Without a parked governor, noting activity does nothing.
        super::note_activity();
        assert!(!super::GOVERNOR_PARKED.load(Ordering::Acquire));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn thp_is_disabled_for_the_process() {
        let t = super::tune_memory();
        assert!(t.thp_disabled);
        // SAFETY: as above.
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) }, 1);
    }
}
