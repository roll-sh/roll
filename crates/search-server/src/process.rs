//! Process-wide memory tuning for nodes that host many mostly-idle clusters.
//!
//! An idle node should cost little, and an unloaded cluster's memory should leave the process.
//! Two allocator/kernel defaults work against that:
//!
//! - **mimalloc's purge delay** (10 ms, times 10 for arenas) keeps freed pages committed for a
//!   while; [`tune_memory`] sets it to 0, as rolld does, so pages go back at once (workers also
//!   run `mi_collect` when they park: [`on_thread_park`]).
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
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
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
