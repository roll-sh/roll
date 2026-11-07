//! The operator's choice between a lean footprint and speed (`roll-engine --mode`). Every knob
//! that trades one for the other takes its value from a `Profile`, derived from the mode here
//! and nowhere else; explicit operator settings (`--cache-mb` and the others) then override
//! single fields. Both modes give the same query results, errors, SQL-visible behaviour and
//! durability (fsync on commit, crash safety): only memory, idle CPU and speed differ.
//!
//! Measured and left out (BENCH.md "Modes"): transparent huge pages (no workload faster, 20 MB
//! more), larger memtables (bulk loads slower), never purging the allocator's free pages (no
//! faster, 80 MB more).

use std::time::Duration;

/// Which way the engine leans where footprint and speed pull apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// The lean footprint: small caches, idle sessions give their memory back, a database
    /// closes a minute after its last session. For test and development environments and
    /// many-branch serverless use, where most servers and sessions sit idle.
    #[default]
    Efficient,
    /// Speed, for production: caches sized from the memory the server may use (the
    /// container's limit, else the machine's), sessions and databases kept warm, a waiting
    /// connection polling for its client while cores are free.
    Performance,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "efficient" => Some(Mode::Efficient),
            "performance" => Some(Mode::Performance),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Efficient => "efficient",
            Mode::Performance => "performance",
        }
    }
}

/// Every footprint-or-speed knob's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub mode: Mode,
    /// The block cache every database reads through.
    pub cache_bytes: usize,
    /// Parse trees, statement templates and PL/pgSQL compiles (`lru::set_plan_cache_bytes`).
    pub plan_cache_bytes: usize,
    /// Close a database this long after its last session ends (`Duration::MAX`: never).
    pub idle_close: Duration,
    /// A connection waiting a while for its client gives its thread's free memory back
    /// (`server::set_idle_hook`): its next statement pays 50-190 µs of CPU taking it again.
    /// Off, idle sessions keep it and never wake.
    pub idle_trim: bool,
    /// How long a connection's thread polls for its client's next message, while cores are
    /// free, before it sleeps (`server::set_wait_spin`; zero: it sleeps at once).
    pub wait_spin: Duration,
}

/// Efficient mode's block cache (what `roll-engine` always had).
pub const EFFICIENT_CACHE_BYTES: usize = 64 << 20;

impl Profile {
    /// The mode's profile on a server that may use `memory` bytes (`server_memory`).
    pub fn new(mode: Mode, memory: u64) -> Profile {
        match mode {
            Mode::Efficient => Profile {
                mode,
                cache_bytes: EFFICIENT_CACHE_BYTES,
                plan_cache_bytes: crate::lru::PLAN_CACHE_BYTES,
                idle_close: Duration::from_secs(60),
                idle_trim: true,
                wait_spin: Duration::ZERO,
            },
            Mode::Performance => {
                // A quarter of the memory for the shared caches, PostgreSQL's shared_buffers
                // guidance, 3 : 1 between blocks and plans as --memory-budget-mb splits it;
                // never below efficient mode's, the plan caches at most 1 GB.
                let budget = (memory / 4).min(usize::MAX as u64) as usize;
                let plans = (budget / 4).clamp(crate::lru::PLAN_CACHE_BYTES, 1 << 30);
                Profile {
                    mode,
                    cache_bytes: (budget - budget / 4).max(EFFICIENT_CACHE_BYTES),
                    plan_cache_bytes: plans,
                    idle_close: Duration::MAX,
                    idle_trim: false,
                    // (A client's next message within it skips the sleep and the wake-up:
                    // one client's select-only 1.46x, TPC-B-like 1.09x; 100 µs gained
                    // no more.)
                    wait_spin: Duration::from_micros(50),
                }
            }
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
        assert_eq!(Mode::parse(" Performance"), Some(Mode::Performance));
        assert_eq!(Mode::parse("fast"), None);
        // Efficient mode is what roll-engine always ran with, whatever the machine.
        let e = Profile::new(Mode::Efficient, 64 << 30);
        assert_eq!((e.cache_bytes, e.plan_cache_bytes), (64 << 20, crate::lru::PLAN_CACHE_BYTES));
        // Performance mode: a quarter of the memory, 3 : 1, within bounds.
        let p = Profile::new(Mode::Performance, 8 << 30);
        assert_eq!((p.cache_bytes, p.plan_cache_bytes), (1536 << 20, 512 << 20));
        let small = Profile::new(Mode::Performance, 256 << 20);
        assert_eq!((small.cache_bytes, small.plan_cache_bytes), (64 << 20, crate::lru::PLAN_CACHE_BYTES));
        assert_eq!(Profile::new(Mode::Performance, 1 << 40).plan_cache_bytes, 1 << 30);
        assert!(server_memory() > 0);
    }
}
