//! Standalone server defaults. Shared caches use a quarter of the available memory;
//! explicit operator settings override individual fields. Sessions and databases stay warm.

use std::time::Duration;

/// The standalone server's resource configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// The block cache every database reads through.
    pub cache_bytes: usize,
    /// Parse trees, statement templates and PL/pgSQL compiles.
    pub plan_cache_bytes: usize,
    /// Close a database this long after its last session ends (`Duration::MAX`: never).
    pub idle_close: Duration,
    /// Poll for the client's next message while cores are free, then sleep.
    pub wait_spin: Duration,
    /// Each database's memtable: written to an L0 file once full, four of which a compaction
    /// merges into L1 (the WAL a crash replays into it is as long). Updates spread over a big
    /// table reach most of its L1 blocks between two compactions, so one memtable four times
    /// as big writes about a quarter as much again: TPC-B-like at scale 32, the flush thread's
    /// 136k user instructions a transaction 64k (HANDOFF "Short write transactions, round 20").
    pub memtable_bytes: usize,
}

impl Profile {
    /// Defaults for a server that may use `memory` bytes (`server_memory`).
    pub fn new(memory: u64) -> Profile {
        // A quarter for shared caches, 3 : 1 between blocks and plans. Keep the existing
        // minimum capacities and cap plans at 1 GiB; capacity is not resident memory.
        let budget = (memory / 4).min(usize::MAX as u64) as usize;
        Profile {
            cache_bytes: (budget - budget / 4).max(64 << 20),
            plan_cache_bytes: (budget / 4).clamp(crate::lru::PLAN_CACHE_BYTES, 1 << 30),
            idle_close: Duration::MAX,
            wait_spin: Duration::from_micros(50),
            // (1/256 of the memory, 8 MB to 32 MB: 32 MB from 8 GiB on.)
            memtable_bytes: (memory / 256).clamp(8 << 20, 32 << 20) as usize,
        }
    }
}

/// The memory this server may use: its cgroup's limit (the tightest of its own and its
/// ancestors', cgroup v2) when it has one, else the machine's.
pub fn server_memory() -> u64 {
    let total = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            let l = m.lines().find(|l| l.starts_with("MemTotal:"))?;
            l.split_whitespace().nth(1)?.parse::<u64>().ok()
        })
        .map_or(8 << 30, |kb| kb << 10);
    let mut limit = total;
    if let Ok(cg) = std::fs::read_to_string("/proc/self/cgroup")
        && let Some(path) = cg.lines().find_map(|l| l.strip_prefix("0::"))
    {
        let mut p = std::path::PathBuf::from("/sys/fs/cgroup");
        p.push(path.trim_start_matches('/'));
        loop {
            if let Ok(v) = std::fs::read_to_string(p.join("memory.max"))
                && let Ok(n) = v.trim().parse::<u64>()
            {
                limit = limit.min(n);
            }
            if p == std::path::Path::new("/sys/fs/cgroup") || !p.pop() {
                break;
            }
        }
    }
    limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles() {
        let p = Profile::new(8 << 30);
        assert_eq!((p.cache_bytes, p.plan_cache_bytes), (1536 << 20, 512 << 20));
        assert_eq!(p.idle_close, Duration::MAX);
        assert_eq!(p.wait_spin, Duration::from_micros(50));
        assert_eq!(p.memtable_bytes, 32 << 20);
        assert_eq!(Profile::new(2 << 30).memtable_bytes, 8 << 20);
        for memory in [0, 256 << 20] {
            let small = Profile::new(memory);
            assert_eq!(
                (small.cache_bytes, small.plan_cache_bytes),
                (64 << 20, crate::lru::PLAN_CACHE_BYTES)
            );
        }
        assert_eq!(Profile::new(1 << 40).plan_cache_bytes, 1 << 30);
        assert!(server_memory() > 0);
    }
}
