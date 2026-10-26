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
//!   worker collect its heap, leaving an idle process as small as before.
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
    #[cfg(target_os = "linux")]
    fn thp_is_disabled_for_the_process() {
        let t = super::tune_memory();
        assert!(t.thp_disabled);
        // SAFETY: as above.
        assert_eq!(unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) }, 1);
    }
}
