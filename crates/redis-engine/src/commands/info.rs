//! INFO: every section and field of Redis 8.10's `genRedisInfoString`, in its order. What only
//! the server around the engine knows (connections, network, replication) comes from [`Host`].

use super::*;
use super::persist::ChildKind;
use super::server::REDIS_VERSION;
use crate::engine::Engine;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

// ---- what the server reports ----

/// Server-level counters and state for INFO, implemented by the server hosting the engine.
pub trait Host: Send + Sync {
    fn counters(&self) -> Counters;
    fn replication(&self) -> Replication;
    /// The write log is durable (`aof_enabled`).
    fn aof_enabled(&self) -> bool {
        false
    }
}

/// Monotonic server counters. CONFIG RESETSTAT doesn't reset them (usage metering reads them);
/// the engine keeps a baseline instead.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub connections: u64,
    pub rejected_connections: u64,
    pub net_in: u64,
    pub net_out: u64,
    pub net_repl_in: u64,
    pub net_repl_out: u64,
    pub reads: u64,
    pub writes: u64,
    /// Read events that parsed commands, and the commands they parsed.
    pub processing_events: u64,
    pub pipeline_sum: u64,
    pub pipeline_cnt: u64,
    pub sync_full: u64,
    pub sync_partial_ok: u64,
    pub sync_partial_err: u64,
}

impl Counters {
    fn since(&self, base: &Counters) -> Counters {
        Counters {
            connections: self.connections.saturating_sub(base.connections),
            rejected_connections: self.rejected_connections.saturating_sub(base.rejected_connections),
            net_in: self.net_in.saturating_sub(base.net_in),
            net_out: self.net_out.saturating_sub(base.net_out),
            net_repl_in: self.net_repl_in.saturating_sub(base.net_repl_in),
            net_repl_out: self.net_repl_out.saturating_sub(base.net_repl_out),
            reads: self.reads.saturating_sub(base.reads),
            writes: self.writes.saturating_sub(base.writes),
            processing_events: self.processing_events.saturating_sub(base.processing_events),
            pipeline_sum: self.pipeline_sum.saturating_sub(base.pipeline_sum),
            pipeline_cnt: self.pipeline_cnt.saturating_sub(base.pipeline_cnt),
            sync_full: self.sync_full.saturating_sub(base.sync_full),
            sync_partial_ok: self.sync_partial_ok.saturating_sub(base.sync_partial_ok),
            sync_partial_err: self.sync_partial_err.saturating_sub(base.sync_partial_err),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Replication {
    pub replid: String,
    pub offset: u64,
    /// The backlog: capacity, offset of its first byte, bytes held.
    pub backlog: Option<(u64, u64, u64)>,
    pub replicas: Vec<ReplicaInfo>,
    pub master: Option<MasterInfo>,
    /// The previous replication id and the last offset it covers.
    pub replid2: Option<(String, u64)>,
}

#[derive(Clone, Debug, Default)]
pub struct ReplicaInfo {
    pub ip: String,
    pub port: u16,
    pub offset: u64,
    pub lag: i64,
}

#[derive(Clone, Debug, Default)]
pub struct MasterInfo {
    pub host: String,
    pub port: u16,
    /// ROLE's link state: connect, connecting, handshake, sync or connected.
    pub state: &'static str,
    pub sync_in_progress: bool,
    /// Set up by the platform (not REPLICAOF): REPLICAOF can't change it.
    pub managed: bool,
    pub link_up: bool,
    /// Seconds since the last data from the primary (-1: never).
    pub last_io: i64,
    pub offset: u64,
    /// Unix seconds the link went down / came up (0: never).
    pub down_since: i64,
    pub up_since: i64,
}

// ---- latency histograms (HdrHistogram, 1ns..1s at 2 significant digits) ----

/// A port of HdrHistogram's indexing with Redis' parameters (lowest 1, highest 10^9,
/// 2 significant figures), counts kept sparse.
#[derive(Clone, Debug, Default)]
pub struct Histogram {
    counts: BTreeMap<i32, u64>,
    total: u64,
}

const SUB_HALF_MAG: i32 = 7;
const SUB_HALF: i64 = 1 << SUB_HALF_MAG;
const SUB_COUNT: i64 = SUB_HALF * 2;
const SUB_MASK: i64 = SUB_COUNT - 1;
const HIST_MAX: i64 = 1_000_000_000;

fn bucket_index(v: i64) -> i32 {
    (64 - (v | SUB_MASK).leading_zeros() as i32) - (SUB_HALF_MAG + 1)
}

fn sub_bucket_index(v: i64, b: i32) -> i64 {
    v >> b
}

fn lowest_equivalent(v: i64) -> i64 {
    let b = bucket_index(v);
    sub_bucket_index(v, b) << b
}

fn highest_equivalent(v: i64) -> i64 {
    let b = bucket_index(v);
    let adjusted = if sub_bucket_index(v, b) >= SUB_COUNT { b + 1 } else { b };
    lowest_equivalent(v) + (1i64 << adjusted) - 1
}

fn value_at_index(i: i32) -> i64 {
    let mut b = (i >> SUB_HALF_MAG) - 1;
    let mut sb = (i as i64 & (SUB_HALF - 1)) + SUB_HALF;
    if b < 0 {
        sb -= SUB_HALF;
        b = 0;
    }
    sb << b
}

impl Histogram {
    /// Record a duration in nanoseconds (clamped to the histogram's range like Redis does).
    pub fn record(&mut self, ns: i64) {
        let v = ns.clamp(1, HIST_MAX);
        let b = bucket_index(v);
        let i = ((b + 1) << SUB_HALF_MAG) + (sub_bucket_index(v, b) - SUB_HALF) as i32;
        *self.counts.entry(i).or_default() += 1;
        self.total += 1;
    }

    /// `hdr_value_at_percentile`.
    pub fn value_at(&self, percentile: f64) -> i64 {
        let p = percentile.min(100.0);
        let want = (((p / 100.0) * self.total as f64) + 0.5) as u64;
        let want = want.max(1);
        let mut seen = 0;
        let mut v = 0;
        for (&i, &n) in &self.counts {
            seen += n;
            if seen >= want {
                v = value_at_index(i);
                break;
            }
        }
        if percentile == 0.0 { lowest_equivalent(v) } else { highest_equivalent(v) }
    }
}

/// `%f` trimmed of trailing zeros (and the dot), like Redis' `trimDoubleString`.
pub fn trim_double(f: f64) -> String {
    let s = format!("{f:.6}");
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_string()
}

// ---- instantaneous metrics (trackInstantaneousMetric) ----

const SAMPLES: usize = 16;

#[derive(Clone, Debug, Default)]
struct Metric {
    samples: [i64; SAMPLES],
    idx: usize,
    last_base: i64,
    last_value: i64,
}

impl Metric {
    fn track(&mut self, value: i64, base: i64, factor: i64) {
        if self.last_base > 0 {
            let b = base - self.last_base;
            let v = value - self.last_value;
            self.samples[self.idx] = if b > 0 { v * factor / b } else { 0 };
            self.idx = (self.idx + 1) % SAMPLES;
        }
        self.last_base = base;
        self.last_value = value;
    }

    fn get(&self) -> i64 {
        self.samples.iter().sum::<i64>() / SAMPLES as i64
    }
}

/// Samples taken every 100 ms by the cron: commands, network, event-loop cycles and duration.
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    command: Metric,
    net_in: Metric,
    net_out: Metric,
    net_repl_in: Metric,
    net_repl_out: Metric,
    el_cycle: Metric,
    el_duration: Metric,
    /// The counters at the last CONFIG RESETSTAT.
    pub(crate) base: Counters,
    /// Event-loop analogue: batches of commands run under one engine lock, and their time (µs).
    pub cycles: u64,
    pub cycles_with_clients: u64,
    pub duration_us: u64,
    pub cmd_us: u64,
    pub duration_max_us: u64,
    pub cmds_per_cycle_max: u64,
    pub cron_us: u64,
}

impl Engine {
    /// Called every 100 ms: instantaneous metrics, peak memory, MIGRATE's idle sockets.
    pub fn cron_metrics(&mut self) {
        let now = crate::util::now_ms();
        let c = self.host.as_ref().map(|h| h.counters()).unwrap_or_default();
        let m = &mut self.metrics;
        m.command.track(self.stats.commands as i64, now, 1000);
        m.net_in.track(c.net_in as i64, now, 1000);
        m.net_out.track(c.net_out as i64, now, 1000);
        m.net_repl_in.track(c.net_repl_in as i64, now, 1000);
        m.net_repl_out.track(c.net_repl_out as i64, now, 1000);
        let us = now * 1000;
        m.el_cycle.track(m.cycles as i64, us, 1_000_000);
        let (cycles, dur) = (m.cycles as i64, m.duration_us as i64);
        // Average cycle duration since the last sample.
        let d = &mut m.el_duration;
        let per_cycle = if cycles > d.last_base { (dur - d.last_value) / (cycles - d.last_base) } else { 0 };
        if d.last_base > 0 || d.last_value > 0 {
            d.samples[d.idx] = per_cycle;
            d.idx = (d.idx + 1) % SAMPLES;
        }
        d.last_base = cycles;
        d.last_value = dur;
        self.update_peak(now);
        self.migrate_sockets.expire();
    }

    /// One batch of commands finished under the engine lock (an event-loop cycle).
    pub fn cycle_done(&mut self, cmds: u64, us: u64) {
        let m = &mut self.metrics;
        m.cycles += 1;
        if cmds > 0 {
            m.cycles_with_clients += 1;
        }
        m.duration_us += us;
        m.duration_max_us = m.duration_max_us.max(us);
        m.cmds_per_cycle_max = m.cmds_per_cycle_max.max(cmds);
    }

    fn update_peak(&mut self, now: i64) {
        let used = self.used_memory();
        if used > self.peak_memory {
            self.peak_memory = used;
            self.peak_time = now / 1000;
        }
    }

    /// CONFIG RESETSTAT.
    pub fn reset_stats(&mut self) {
        self.stats = Default::default();
        self.stats.total_blocking_keys = self.blocking.by_key.len() as u64;
        self.cmdstats.clear();
        self.errorstats.clear();
        let base = self.host.as_ref().map(|h| h.counters()).unwrap_or_default();
        self.metrics = Metrics { base, ..Default::default() };
        let p = &mut self.persist;
        (p.aof_rewrites, p.total_forks, p.latest_fork_us) = (0, 0, 0);
        self.hub.obuf_disconnections.store(0, std::sync::atomic::Ordering::Relaxed);
        self.hub.qbuf_disconnections.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

// ---- section selection ----

const DEFAULT: &[&str] =
    &["server", "clients", "memory", "persistence", "stats", "replication", "threads", "cpu", "hotkeys", "module_list", "errorstats", "cluster", "keyspace", "keysizes"];

struct Want {
    names: BTreeSet<String>,
    all: bool,
    everything: bool,
}

impl Want {
    fn of(args: &[Bytes]) -> Self {
        let mut w = Want { names: BTreeSet::new(), all: false, everything: false };
        if args.len() <= 1 {
            w.names.extend(DEFAULT.iter().map(|s| s.to_string()));
            return w;
        }
        for a in &args[1..] {
            let a = String::from_utf8_lossy(a).to_ascii_lowercase();
            match a.as_str() {
                "default" => w.names.extend(DEFAULT.iter().map(|s| s.to_string())),
                "all" => w.all = true,
                "everything" => {
                    w.all = true;
                    w.everything = true;
                }
                _ => {
                    w.names.insert(a);
                }
            }
        }
        w
    }

    fn has(&self, name: &str) -> bool {
        self.all || self.names.contains(name)
    }
}

// ---- formatting helpers ----

/// `bytesToHuman`.
pub fn human(n: u64) -> String {
    let d = n as f64;
    const K: f64 = 1024.0;
    if n < 1024 {
        format!("{n}B")
    } else if d < K * K {
        format!("{:.2}K", d / K)
    } else if d < K * K * K {
        format!("{:.2}M", d / K / K)
    } else if d < K * K * K * K {
        format!("{:.2}G", d / K / K / K)
    } else if d < K * K * K * K * K {
        format!("{:.2}T", d / K / K / K / K)
    } else if d < K * K * K * K * K * K {
        format!("{:.2}P", d / K / K / K / K / K)
    } else {
        format!("{n}B")
    }
}

fn run_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let mut rng = crate::util::Rng::new();
        (0..40).map(|_| char::from_digit((rng.next_u64() % 16) as u32, 16).unwrap()).collect()
    })
}

fn uname() -> String {
    #[cfg(unix)]
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) == 0 {
            let f = |b: &[libc::c_char]| std::ffi::CStr::from_ptr(b.as_ptr()).to_string_lossy().into_owned();
            return format!("{} {} {}", f(&u.sysname), f(&u.release), f(&u.machine));
        }
    }
    std::env::consts::OS.to_string()
}

fn rusage(who: libc::c_int) -> (String, String) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(who, &mut ru) };
    let t = |tv: libc::timeval| format!("{}.{:06}", tv.tv_sec, tv.tv_usec);
    (t(ru.ru_stime), t(ru.ru_utime))
}

/// Resident set size of the process.
fn process_rss() -> u64 {
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/proc/self/statm")
        && let Some(pages) = s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok())
    {
        return pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    }
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    // Max RSS: kilobytes on Linux, bytes on macOS.
    if cfg!(target_os = "macos") { ru.ru_maxrss as u64 } else { ru.ru_maxrss as u64 * 1024 }
}

pub fn system_memory() -> u64 {
    unsafe {
        let pages = libc::sysconf(libc::_SC_PHYS_PAGES);
        let size = libc::sysconf(libc::_SC_PAGESIZE);
        if pages > 0 && size > 0 { pages as u64 * size as u64 } else { 0 }
    }
}

const SIZE_LABELS: [&str; 63] = [
    "0", "1", "2", "4", "8", "16", "32", "64", "128", "256", "512", "1K", "2K", "4K", "8K", "16K", "32K", "64K", "128K", "256K", "512K", "1M", "2M",
    "4M", "8M", "16M", "32M", "64M", "128M", "256M", "512M", "1G", "2G", "4G", "8G", "16G", "32G", "64G", "128G", "256G", "512G", "1T", "2T", "4T",
    "8T", "16T", "32T", "64T", "128T", "256T", "512T", "1P", "2P", "4P", "8P", "16P", "32P", "64P", "128P", "256P", "512P", "1E", "2E",
];

// ---- the command ----

/// INFO [section [section …]]
pub fn info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let want = Want::of(args);
    let port = cx.client.laddr.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(6379);
    let text = generate(cx.eng, &want, cx.now, port);
    Ok(Reply::Verbatim("txt", Bytes::from(text)))
}

/// The INFO text for `sections` (as given to INFO), for callers outside a command.
pub fn info_text(eng: &mut Engine, sections: &[&str], now: i64) -> String {
    let mut args = vec![Bytes::from_static(b"info")];
    args.extend(sections.iter().map(|s| Bytes::copy_from_slice(s.as_bytes())));
    generate(eng, &Want::of(&args), now, 6379)
}

fn generate(eng: &mut Engine, want: &Want, now: i64, port: u16) -> String {
    let mut s = String::new();
    let mut sections = 0;
    let mut begin = |s: &mut String, header: &str| {
        if sections > 0 {
            s.push_str("\r\n");
        }
        sections += 1;
        s.push_str(header);
        s.push_str("\r\n");
    };
    let counters = eng.host.as_ref().map(|h| h.counters()).unwrap_or_default().since(&eng.metrics.base);
    let uptime = (now - eng.start_ms) / 1000;

    if want.has("server") {
        begin(&mut s, "# Server");
        let bind = crate::config::find("bind").map_or_else(|| "* -::*".to_string(), |p| p.get(&eng.cfg));
        let _ = write!(
            s,
            "redis_version:{REDIS_VERSION}\r\nredis_git_sha1:00000000\r\nredis_git_dirty:0\r\nredis_build_id:{:016x}\r\nredis_mode:standalone\r\nos:{}\r\narch_bits:{}\r\n\
             monotonic_clock:POSIX clock_gettime\r\nmultiplexing_api:{}\r\natomicvar_api:c11-builtin\r\ngcc_version:0.0.0\r\nprocess_id:{}\r\nprocess_supervised:no\r\nrun_id:{}\r\n\
             tcp_port:{port}\r\nserver_time_usec:{}\r\nuptime_in_seconds:{uptime}\r\nuptime_in_days:{}\r\nhz:{}\r\nconfigured_hz:{}\r\nlru_clock:{}\r\nexecutable:{}\r\nconfig_file:\r\nio_threads_active:0\r\n",
            REDIS_VERSION.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3)),
            uname(),
            usize::BITS,
            if cfg!(target_os = "linux") { "epoll" } else if cfg!(any(target_os = "macos", target_os = "freebsd")) { "kqueue" } else { "select" },
            std::process::id(),
            run_id(),
            crate::util::now_us(),
            uptime / 86400,
            eng.cfg.hz,
            eng.cfg.hz,
            Engine::lru_clock(now),
            std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
        );
        s.push_str("listener0:name=tcp");
        for b in bind.split_whitespace() {
            let _ = write!(s, ",bind={b}");
        }
        let _ = write!(s, ",port={port}\r\n");
    }

    if want.has("clients") {
        begin(&mut s, "# Clients");
        let cl = &eng.clients;
        let (mut max_in, mut max_out, mut postponed, mut pubsub, mut active) = (0u64, 0u64, 0usize, 0usize, 0usize);
        let window = (now / 128) * 128 - 3 * 128;
        for e in cl.by_id.values() {
            max_in = max_in.max(e.stats.qbuf.load(Ordering::Relaxed));
            max_out = max_out.max(e.stats.obuf.load(Ordering::Relaxed));
            postponed += e.stats.postponed.load(Ordering::Relaxed) as usize;
            pubsub += (e.view.sub + e.view.psub + e.view.ssub > 0) as usize;
            let last = e.stats.last_cmd_ms.load(Ordering::Relaxed);
            active += (last > 0 && (last / 128) * 128 >= window) as usize;
        }
        let b = &eng.blocking;
        let in_timeout = b.waiters.values().filter(|w| w.deadline > 0 || w.wake_at > 0).count();
        let on_nokey = b.by_key.values().filter(|q| q.iter().any(|id| b.waiters.get(id).is_some_and(|w| w.unblock_on_nokey))).count();
        let _ = write!(
            s,
            "connected_clients:{}\r\ncluster_connections:0\r\nmaxclients:{}\r\nclient_recent_max_input_buffer:{max_in}\r\nclient_recent_max_output_buffer:{max_out}\r\n\
             blocked_clients:{}\r\ntracking_clients:{}\r\npubsub_clients:{pubsub}\r\nwatching_clients:{}\r\nclients_in_timeout_table:{in_timeout}\r\nactive_clients:{active}\r\n\
             total_watched_keys:{}\r\ntotal_blocking_keys:{}\r\ntotal_blocking_keys_on_nokey:{on_nokey}\r\n",
            cl.by_id.len(),
            eng.cfg.maxclients,
            b.waiters.len() + postponed,
            cl.tracking_clients,
            eng.watches.by_client.len(),
            eng.watches.keys.len(),
            b.by_key.len(),
        );
    }

    if want.has("memory") {
        begin(&mut s, "# Memory");
        eng.update_peak(now);
        let used = eng.used_memory() as u64;
        let rss = process_rss();
        let peak = eng.peak_memory as u64;
        let sc = &eng.scripting;
        let (vm_eval, vm_functions) = (sc.memory() as u64, sc.functions_memory() as u64);
        let (scripts_eval, functions) = (sc.scripts_memory() as u64, sc.libraries_memory() as u64);
        let clients_mem = eng.clients_memory();
        let tmpl_mem = eng.hub.templates.stats().mem;
        let startup = eng.startup_memory as u64;
        let keys_overhead: u64 = eng.dbs.iter().map(|d| (d.len() * crate::value::KEY_OVERHEAD + d.keys.table_bytes() + d.volatile.table_bytes()) as u64).sum();
        let overhead = (startup + clients_mem + eng.overhead as u64 + keys_overhead + scripts_eval + functions).min(used.max(startup));
        let dataset = used.saturating_sub(overhead);
        let net = used.saturating_sub(startup);
        let pct = |a: u64, b: u64| if b > 0 { a as f64 * 100.0 / b as f64 } else { 0.0 };
        let ratio = |a: u64, b: u64| if b > 0 { a as f64 / b as f64 } else { 0.0 };
        let total = system_memory();
        let _ = write!(
            s,
            "used_memory:{used}\r\nused_memory_human:{}\r\nused_memory_rss:{rss}\r\nused_memory_rss_human:{}\r\nused_memory_peak:{peak}\r\nused_memory_peak_human:{}\r\n\
             used_memory_peak_time:{}\r\nused_memory_peak_perc:{:.2}%\r\nused_memory_overhead:{overhead}\r\nused_memory_startup:{startup}\r\nused_memory_dataset:{dataset}\r\n\
             used_memory_dataset_perc:{:.2}%\r\nallocator_allocated:{used}\r\nallocator_active:{used}\r\nallocator_resident:{rss}\r\nallocator_muzzy:0\r\n\
             total_system_memory:{total}\r\ntotal_system_memory_human:{}\r\nused_memory_lua:{vm_eval}\r\nused_memory_vm_eval:{vm_eval}\r\nused_memory_lua_human:{}\r\n\
             used_memory_scripts_eval:{scripts_eval}\r\nnumber_of_cached_scripts:{}\r\nnumber_of_functions:{}\r\nnumber_of_libraries:{}\r\nused_memory_vm_functions:{vm_functions}\r\n\
             used_memory_vm_total:{}\r\nused_memory_vm_total_human:{}\r\nused_memory_functions:{functions}\r\nused_memory_hash_templates:{tmpl_mem}\r\nused_memory_scripts:{}\r\n\
             used_memory_scripts_human:{}\r\nmaxmemory:{}\r\nmaxmemory_human:{}\r\nmaxmemory_policy:{}\r\nallocator_frag_ratio:1.00\r\nallocator_frag_bytes:0\r\n\
             allocator_rss_ratio:{:.2}\r\nallocator_rss_bytes:{}\r\nrss_overhead_ratio:1.00\r\nrss_overhead_bytes:0\r\nmem_fragmentation_ratio:{:.2}\r\nmem_fragmentation_bytes:{}\r\n\
             mem_not_counted_for_evict:0\r\nmem_replication_backlog:{}\r\nmem_total_replication_buffers:0\r\nmem_replica_full_sync_buffer:0\r\nmem_clients_slaves:0\r\n\
             mem_clients_normal:{clients_mem}\r\nmem_clients_normal_shared:0\r\nmem_clients_normal_unshared:{clients_mem}\r\nmem_cluster_slot_migration_output_buffer:0\r\n\
             mem_cluster_slot_migration_input_buffer:0\r\nmem_cluster_slot_migration_input_buffer_peak:0\r\nmem_cluster_links:0\r\nmem_aof_buffer:0\r\nmem_allocator:libc\r\n\
             mem_overhead_db_hashtable_rehashing:0\r\nactive_defrag_running:0\r\nlazyfree_pending_objects:0\r\nlazyfreed_objects:0\r\n",
            human(used),
            human(rss),
            human(peak),
            eng.peak_time,
            pct(used, peak),
            pct(dataset, net),
            human(total),
            human(vm_eval),
            sc.cached_scripts(),
            sc.functions_count(),
            sc.libraries().len(),
            vm_eval + vm_functions,
            human(vm_eval + vm_functions),
            scripts_eval + functions,
            human(scripts_eval + functions),
            eng.cfg.maxmemory,
            human(eng.cfg.maxmemory),
            eng.cfg.maxmemory_policy.name(),
            ratio(rss, used),
            rss as i64 - used as i64,
            ratio(rss, used),
            rss as i64 - used as i64,
            eng.host.as_ref().and_then(|h| h.replication().backlog).map_or(0, |b| b.2),
        );
    }

    if want.has("persistence") {
        begin(&mut s, "# Persistence");
        let aof = eng.persist.aof != crate::commands::persist::AofState::Off || eng.host.as_ref().is_some_and(|h| h.aof_enabled());
        let (p, now_s) = (&eng.persist, now / 1000);
        let _ = write!(
            s,
            "loading:0\r\nasync_loading:0\r\ncurrent_cow_peak:0\r\ncurrent_cow_size:0\r\ncurrent_cow_size_age:0\r\ncurrent_fork_perc:0.00\r\ncurrent_save_keys_processed:0\r\n\
             current_save_keys_total:{}\r\nrdb_changes_since_last_save:{}\r\nrdb_bgsave_in_progress:{}\r\nrdb_last_save_time:{}\r\nrdb_last_bgsave_status:ok\r\n\
             rdb_last_bgsave_time_sec:{}\r\nrdb_current_bgsave_time_sec:{}\r\nrdb_saves:{}\r\nrdb_saves_consecutive_failures:0\r\nrdb_last_cow_size:0\r\n\
             rdb_last_load_keys_expired:{}\r\nrdb_last_load_keys_loaded:{}\r\naof_enabled:{}\r\naof_rewrite_in_progress:{}\r\naof_rewrite_scheduled:{}\r\n\
             aof_last_rewrite_time_sec:{}\r\naof_current_rewrite_time_sec:{}\r\naof_last_bgrewrite_status:ok\r\naof_rewrites:{}\r\naof_rewrites_consecutive_failures:0\r\n\
             aof_last_write_status:ok\r\naof_last_cow_size:0\r\nbackup_in_progress:{}\r\nmodule_fork_in_progress:0\r\nmodule_fork_last_cow_size:0\r\n",
            p.child.as_ref().map_or(0, |c| c.keys),
            eng.dirty(),
            p.in_progress(ChildKind::Rdb) as i32,
            eng.lastsave,
            p.rdb_last_time,
            p.current_time(ChildKind::Rdb, now_s),
            eng.stats.rdb_saves,
            p.last_load.1,
            p.last_load.0,
            aof as i32,
            p.in_progress(ChildKind::Aof) as i32,
            p.aof_rewrite_scheduled as i32,
            p.aof_last_time,
            p.current_time(ChildKind::Aof, now_s),
            p.aof_rewrites,
            p.backup.in_progress() as i32,
        );
        if eng.cfg.appendonly || eng.host.as_ref().is_some_and(|h| h.aof_enabled()) {
            let size = eng.host.as_ref().map_or(0, |h| h.replication().offset);
            let _ = write!(
                s,
                "aof_current_size:{size}\r\naof_base_size:0\r\naof_pending_rewrite:{}\r\naof_buffer_length:0\r\naof_pending_bio_fsync:0\r\naof_delayed_fsync:0\r\n",
                p.aof_rewrite_scheduled as i32,
            );
        }
    }

    if want.has("threads") {
        begin(&mut s, "# Threads");
        let _ = write!(s, "io_thread_0:clients={},reads={},writes={}\r\n", eng.clients.by_id.len(), counters.reads, counters.writes);
    }

    if want.has("stats") {
        begin(&mut s, "# Stats");
        let st = &eng.stats;
        let m = &eng.metrics;
        let kbps = |x: &Metric| x.get() as f64 / 1024.0;
        let acl = eng.acl.read().log.lock().denied;
        let obuf_disconnections = eng.hub.obuf_disconnections.load(Ordering::Relaxed);
        let qbuf_disconnections = eng.hub.qbuf_disconnections.load(Ordering::Relaxed);
        let _ = write!(
            s,
            "total_connections_received:{}\r\ntotal_commands_processed:{}\r\ninstantaneous_ops_per_sec:{}\r\ntotal_net_input_bytes:{}\r\ntotal_net_output_bytes:{}\r\n\
             total_net_repl_input_bytes:{}\r\ntotal_net_repl_output_bytes:{}\r\ninstantaneous_input_kbps:{:.2}\r\ninstantaneous_output_kbps:{:.2}\r\n\
             instantaneous_input_repl_kbps:{:.2}\r\ninstantaneous_output_repl_kbps:{:.2}\r\nrejected_connections:{}\r\nsync_full:{}\r\nsync_partial_ok:{}\r\nsync_partial_err:{}\r\n\
             expired_subkeys:{}\r\nexpired_subkeys_active:{}\r\nexpired_keys:{}\r\nexpired_keys_active:{}\r\nexpired_stale_perc:{:.2}\r\nexpired_time_cap_reached_count:{}\r\n\
             expire_cycle_cpu_milliseconds:{}\r\nevicted_keys:{}\r\nevicted_clients:{}\r\nevicted_scripts:{}\r\ntotal_eviction_exceeded_time:{}\r\ncurrent_eviction_exceeded_time:0\r\n\
             keyspace_hits:{}\r\nkeyspace_misses:{}\r\npubsub_channels:{}\r\npubsub_patterns:{}\r\npubsubshard_channels:{}\r\nlatest_fork_usec:{}\r\ntotal_forks:{}\r\n\
             migrate_cached_sockets:{}\r\nslave_expires_tracked_keys:0\r\nactive_defrag_hits:0\r\nactive_defrag_misses:0\r\nactive_defrag_key_hits:0\r\nactive_defrag_key_misses:0\r\n\
             total_active_defrag_time:0\r\ncurrent_active_defrag_time:0\r\ntracking_total_keys:{}\r\ntracking_total_items:{}\r\ntracking_total_prefixes:{}\r\n\
             unexpected_error_replies:{}\r\ntotal_error_replies:{}\r\ndump_payload_sanitizations:0\r\ntotal_reads_processed:{}\r\ntotal_writes_processed:{}\r\n\
             io_threaded_reads_processed:0\r\nio_threaded_writes_processed:0\r\nio_threaded_total_prefetch_batches:0\r\nio_threaded_total_prefetch_entries:0\r\n\
             client_query_buffer_limit_disconnections:{qbuf_disconnections}\r\nclient_output_buffer_limit_disconnections:{obuf_disconnections}\r\nreply_buffer_shrinks:{}\r\nreply_buffer_expands:{}\r\n\
             eventloop_cycles:{}\r\neventloop_duration_sum:{}\r\neventloop_duration_cmd_sum:{}\r\ninstantaneous_eventloop_cycles_per_sec:{}\r\ninstantaneous_eventloop_duration_usec:{}\r\n\
             eventloop_cycles_with_clients_processing:{}\r\ntotal_client_processing_events:{}\r\navg_pipeline_length_sum:{}\r\navg_pipeline_length_cnt:{}\r\navg_pipeline_length:{:.2}\r\n\
             slowlog_commands_count:{}\r\nslowlog_commands_time_ms_max:{:.2}\r\nslowlog_commands_time_ms_sum:{:.2}\r\nhash_templates:{}\r\nhash_template_keys:{}\r\n\
             acl_access_denied_auth:{}\r\nacl_access_denied_cmd:{}\r\nacl_access_denied_key:{}\r\nacl_access_denied_channel:{}\r\nacl_access_denied_tls_cert:0\r\n",
            counters.connections,
            st.commands,
            m.command.get(),
            counters.net_in + counters.net_repl_in,
            counters.net_out + counters.net_repl_out,
            counters.net_repl_in,
            counters.net_repl_out,
            kbps(&m.net_in),
            kbps(&m.net_out),
            kbps(&m.net_repl_in),
            kbps(&m.net_repl_out),
            counters.rejected_connections,
            counters.sync_full,
            counters.sync_partial_ok,
            counters.sync_partial_err,
            st.expired_subkeys,
            st.expired_subkeys_active,
            st.expired_keys,
            st.expired_keys_active,
            st.expired_stale_perc * 100.0,
            st.expired_time_cap_reached,
            st.expire_cycle_us / 1000,
            st.evicted_keys,
            st.evicted_clients,
            st.evicted_scripts,
            st.total_eviction_exceeded_time_ms,
            st.keyspace_hits,
            st.keyspace_misses,
            eng.hub.channels(crate::hub::Kind::Channel, None).len(),
            eng.hub.numpat(),
            eng.hub.channels(crate::hub::Kind::Shard, None).len(),
            eng.persist.latest_fork_us,
            eng.persist.total_forks,
            eng.migrate_sockets.len(),
            eng.clients.tracking_keys(),
            eng.clients.tracking_items(),
            eng.clients.tracking_prefixes(),
            st.unexpected_error_replies,
            st.error_replies,
            counters.reads,
            counters.writes,
            st.reply_buffer_shrinks,
            st.reply_buffer_expands,
            m.cycles,
            m.duration_us,
            m.cmd_us,
            m.el_cycle.get(),
            m.el_duration.get(),
            m.cycles_with_clients,
            counters.processing_events,
            counters.pipeline_sum,
            counters.pipeline_cnt,
            if counters.pipeline_cnt > 0 { counters.pipeline_sum as f64 / counters.pipeline_cnt as f64 } else { 0.0 },
            st.slowlog_count,
            st.slowlog_us_max as f64 / 1000.0,
            st.slowlog_us_sum as f64 / 1000.0,
            eng.hub.templates.stats().templates,
            eng.hub.templates.stats().keys,
            acl[0],
            acl[1],
            acl[2],
            acl[3],
        );
    }

    if want.has("replication") {
        begin(&mut s, "# Replication");
        let r = eng.host.as_ref().map(|h| h.replication()).unwrap_or_default();
        let _ = write!(s, "role:{}\r\n", if eng.replica { "slave" } else { "master" });
        if eng.replica {
            let m = r.master.clone().unwrap_or_default();
            let unix = now / 1000;
            let _ = write!(
                s,
                "master_host:{}\r\nmaster_port:{}\r\nmaster_link_status:{}\r\nmaster_last_io_seconds_ago:{}\r\nmaster_sync_in_progress:{}\r\nslave_read_repl_offset:{}\r\n\
                 slave_repl_offset:{}\r\nreplica_full_sync_buffer_size:0\r\nreplica_full_sync_buffer_peak:0\r\nmaster_current_sync_attempts:0\r\nmaster_total_sync_attempts:1\r\n",
                m.host,
                m.port,
                if m.link_up { "up" } else { "down" },
                if m.link_up { m.last_io } else { -1 },
                m.sync_in_progress as i32,
                m.offset,
                m.offset,
            );
            if m.link_up {
                let _ = write!(s, "master_link_up_since_seconds:{}\r\nmaster_client_io_thread:0\r\n", if m.up_since > 0 { unix - m.up_since } else { -1 });
            } else {
                let _ = write!(s, "master_link_down_since_seconds:{}\r\n", if m.down_since > 0 { unix - m.down_since } else { -1 });
            }
            let down = if !m.link_up && m.down_since > 0 { unix - m.down_since } else { 0 };
            let _ = write!(s, "total_disconnect_time_sec:{down}\r\nslave_priority:100\r\nslave_read_only:1\r\nreplica_announced:1\r\n");
        }
        let _ = write!(s, "connected_slaves:{}\r\n", r.replicas.len());
        for (i, rep) in r.replicas.iter().enumerate() {
            let _ = write!(s, "slave{i}:ip={},port={},state=online,offset={},lag={},io-thread=0\r\n", rep.ip, rep.port, rep.offset, rep.lag);
        }
        let replid = if r.replid.is_empty() { run_id().to_string() } else { r.replid.clone() };
        let (active, size, first, hist) = match r.backlog {
            Some((size, first, hist)) => (1, size, first, hist),
            None => (0, 1048576, 0, 0),
        };
        let _ = write!(
            s,
            "master_failover_state:no-failover\r\nmaster_replid:{replid}\r\nmaster_replid2:{}\r\nmaster_repl_offset:{}\r\n\
             second_repl_offset:{}\r\nrepl_backlog_active:{active}\r\nrepl_backlog_size:{size}\r\nrepl_backlog_first_byte_offset:{first}\r\nrepl_backlog_histlen:{hist}\r\n",
            r.replid2.as_ref().map_or("0000000000000000000000000000000000000000", |(id, _)| id.as_str()),
            r.offset,
            r.replid2.as_ref().map_or(-1, |(_, o)| *o as i64),
        );
    }

    if want.has("cpu") {
        begin(&mut s, "# CPU");
        let (sys, user) = rusage(libc::RUSAGE_SELF);
        let (csys, cuser) = rusage(libc::RUSAGE_CHILDREN);
        let _ = write!(s, "used_cpu_sys:{sys}\r\nused_cpu_user:{user}\r\nused_cpu_sys_children:{csys}\r\nused_cpu_user_children:{cuser}\r\n");
        // The calling thread's usage where the OS reports it per thread (Linux), else the process'.
        #[cfg(target_os = "linux")]
        let (tsys, tuser) = rusage(libc::RUSAGE_THREAD);
        #[cfg(not(target_os = "linux"))]
        let (tsys, tuser) = (sys, user);
        let _ = write!(s, "used_cpu_sys_main_thread:{tsys}\r\nused_cpu_user_main_thread:{tuser}\r\n");
    }

    if want.has("hotkeys") {
        begin(&mut s, "# Hotkeys");
        if let Some(h) = &eng.hotkeys {
            let _ = write!(s, "hotkeys-tracking-active:{}\r\nhotkeys-cmd-cpu-time:{}\r\n", h.active as i32, h.cpu_time);
        }
    }

    if want.has("module_list") || want.names.contains("modules") {
        begin(&mut s, "# Modules");
        for m in crate::modules::LIST {
            let _ = write!(
                s,
                "module:name={},ver={},api=1,filters=0,usedby=[{}],using=[{}],options=[{}]\r\n",
                m.name,
                m.ver,
                m.usedby.join("|"),
                m.using.join("|"),
                m.options.join("|")
            );
        }
    }

    if want.has("commandstats") {
        begin(&mut s, "# Commandstats");
        for (k, v) in sorted(&eng.cmdstats) {
            if v.calls == 0 && v.failed == 0 && v.rejected == 0 {
                continue;
            }
            let per = if v.calls > 0 { v.usec as f64 / v.calls as f64 } else { 0.0 };
            let _ = write!(s, "cmdstat_{k}:calls={},usec={},usec_per_call={per:.2},rejected_calls={},failed_calls={}", v.calls, v.usec, v.rejected, v.failed);
            if v.slowlog_count > 0 {
                let _ = write!(
                    s,
                    ",slowlog_count={},slowlog_time_ms_sum={:.2},slowlog_time_ms_max={:.2}",
                    v.slowlog_count,
                    v.slowlog_us_sum as f64 / 1000.0,
                    v.slowlog_us_max as f64 / 1000.0
                );
            }
            s.push_str("\r\n");
        }
    }

    if want.has("errorstats") {
        begin(&mut s, "# Errorstats");
        let mut errs: Vec<_> = eng.errorstats.iter().collect();
        errs.sort();
        for (k, v) in errs {
            // `getSafeInfoString`.
            let k: String = k.chars().map(|c| if matches!(c, '#' | ':' | '\n' | '\r') { '_' } else { c }).collect();
            let _ = write!(s, "errorstat_{k}:count={v}\r\n");
        }
    }

    if want.has("latencystats") {
        begin(&mut s, "# Latencystats");
        if eng.cfg.latency_tracking {
            let ps = eng.cfg.latency_percentiles.clone();
            for (k, v) in sorted(&eng.cmdstats) {
                let Some(h) = &v.latency else { continue };
                let _ = write!(s, "latency_percentiles_usec_{k}:");
                for (i, p) in ps.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    let _ = write!(s, "p{}={:.3}", trim_double(*p), h.value_at(*p) as f64 / 1000.0);
                }
                s.push_str("\r\n");
            }
        }
    }

    if want.has("cluster") {
        begin(&mut s, "# Cluster");
        s.push_str("cluster_enabled:0\r\n");
    }

    if want.has("keyspace") {
        begin(&mut s, "# Keyspace");
        for (i, d) in eng.dbs.iter().enumerate() {
            if !d.is_empty() || d.expires() > 0 {
                let _ = write!(s, "db{i}:keys={},expires={},avg_ttl={},subexpiry={}\r\n", d.len(), d.expires(), d.avg_ttl, d.subexpires.len());
            }
        }
    }

    if want.has("keysizes") {
        begin(&mut s, "# Keysizes");
        const NAMES: [&str; 5] = ["distrib_strings_sizes", "distrib_lists_items", "distrib_sets_items", "distrib_zsets_items", "distrib_hashes_items"];
        for (i, d) in eng.dbs.iter().enumerate() {
            if d.is_empty() {
                continue;
            }
            for (t, name) in NAMES.iter().enumerate() {
                let bins = &d.keysizes[t * crate::value::SIZE_BINS..(t + 1) * crate::value::SIZE_BINS];
                if bins.iter().all(|&n| n == 0) {
                    continue;
                }
                let _ = write!(s, "db{i}_{name}:");
                let mut first = true;
                for (b, &n) in bins.iter().enumerate() {
                    if n > 0 {
                        let _ = write!(s, "{}{}={n}", if first { "" } else { "," }, SIZE_LABELS[b]);
                        first = false;
                    }
                }
                s.push_str("\r\n");
            }
        }
    }

    // The modules' own sections (`modulesCollectInfo`): only search registers an INFO callback.
    // Given for "everything" and "modules", else for the sections named (or the module's name).
    let pick = want.everything || want.names.contains("modules");
    if pick || want.names.iter().any(|n| n.starts_with("search")) {
        for (name, body) in crate::search::info::sections(&eng.cfg) {
            if pick || want.names.contains(name) || want.names.contains("search") {
                begin(&mut s, &format!("# {name}"));
                s.push_str(&body);
            }
        }
    }

    if want.names.contains("debug") {
        begin(&mut s, "# Debug");
        let m = &eng.metrics;
        let _ = write!(
            s,
            "eventloop_duration_aof_sum:0\r\neventloop_duration_cron_sum:{}\r\neventloop_duration_max:{}\r\neventloop_cmd_per_cycle_max:{}\r\nallocator_allocated_lua:{}\r\n\
             allocator_active_lua:{}\r\nallocator_resident_lua:{}\r\nallocator_frag_bytes_lua:0\r\n",
            m.cron_us,
            m.duration_max_us,
            m.cmds_per_cycle_max,
            eng.scripting.memory(),
            eng.scripting.memory(),
            eng.scripting.memory(),
        );
    }
    s
}

fn sorted(m: &std::collections::HashMap<String, crate::engine::CmdStat>) -> Vec<(&String, &crate::engine::CmdStat)> {
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|a, b| a.0.cmp(b.0));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr_percentiles_follow_its_buckets() {
        let mut h = Histogram::default();
        for v in [1_000, 2_000, 3_000, 1_000_000] {
            h.record(v);
        }
        // 1000 ns sits in a bucket of width 4 (1000..=1003), 2000 ns in one of width 8.
        assert_eq!(h.value_at(50.0), 2_007);
        assert_eq!(h.value_at(25.0), 1_003);
        assert_eq!(h.value_at(100.0), highest_equivalent(1_000_000));
        assert_eq!(h.value_at(0.0), lowest_equivalent(1_000));
        assert_eq!(Histogram::default().value_at(50.0), 0);
        // Values are clamped to 1ns..1s.
        let mut c = Histogram::default();
        c.record(0);
        c.record(5_000_000_000);
        assert_eq!(c.value_at(1.0), 1);
        assert_eq!(c.value_at(100.0), highest_equivalent(HIST_MAX));
    }

    #[test]
    fn trims_doubles_like_redis() {
        assert_eq!(trim_double(50.0), "50");
        assert_eq!(trim_double(99.9), "99.9");
        assert_eq!(trim_double(0.0), "0");
        assert_eq!(trim_double(12.125), "12.125");
    }

    #[test]
    fn humanizes_bytes() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(1023), "1023B");
        assert_eq!(human(1536), "1.50K");
        assert_eq!(human(5 << 20), "5.00M");
        assert_eq!(human(3 << 40), "3.00T");
    }
}
