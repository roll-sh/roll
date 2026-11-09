//! Process and host figures for `/api/nodes` and `/metrics` (memory, file descriptors, disk).

use std::path::Path;

/// Resident memory of this process in bytes.
pub fn rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/statm")
            && let Some(pages) = s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok())
        {
            return pages * page_size();
        }
    }
    // Peak RSS is the best portable approximation elsewhere.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: getrusage writes into the struct we own.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return 0;
    }
    let max = ru.ru_maxrss.max(0) as u64;
    if cfg!(target_os = "macos") { max } else { max * 1024 }
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 { p as u64 } else { 4096 }
}

/// Memory available to this process in bytes: the host's, or the container's limit if lower.
pub fn total_memory() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let host = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
            .map(|kb| kb * 1024)
            .unwrap_or(0);
        // cgroup v2, then v1 ("max" or a huge number when unlimited).
        let limit = ["/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok()?.trim().parse::<u64>().ok());
        match limit {
            Some(l) if l > 0 && (host == 0 || l < host) => l,
            _ => host,
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut v: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: the name is NUL-terminated and `v`/`len` describe a valid u64 buffer.
        let rc = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), (&mut v as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
        if rc == 0 { v } else { 0 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

/// Open file descriptors of this process.
pub fn open_fds() -> u64 {
    for dir in ["/proc/self/fd", "/dev/fd"] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            return rd.count().saturating_sub(1) as u64;
        }
    }
    0
}

/// The file descriptor limit.
pub fn max_fds() -> u64 {
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: getrlimit writes into the struct we own.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0 {
        return 0;
    }
    if rl.rlim_cur == libc::RLIM_INFINITY { 1_048_576 } else { rl.rlim_cur as u64 }
}

/// Free bytes on the filesystem holding `path`.
pub fn disk_free(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return 0 };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `st` is a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    (st.f_bavail as u64).saturating_mul(st.f_frsize as u64)
}

/// Available CPU cores.
pub fn processors() -> u64 {
    std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_process_figures() {
        assert!(rss_bytes() > 0);
        assert!(max_fds() > 0);
        assert!(disk_free(Path::new(".")) > 0);
        assert!(processors() >= 1);
    }
}
