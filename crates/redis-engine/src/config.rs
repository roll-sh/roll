//! Runtime configuration (`CONFIG GET` / `CONFIG SET`), per database.
//!
//! Every parameter real Redis exposes is listed so tools that probe configuration work. Settings
//! that only make sense for a self-managed server (ports, paths, persistence knobs) are readable
//! but immutable: the platform owns them.

use crate::notify;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    NoEviction,
    AllKeysLru,
    AllKeysLfu,
    AllKeysRandom,
    VolatileLru,
    VolatileLfu,
    VolatileRandom,
    VolatileTtl,
    /// Least recently modified (Redis 8.6+).
    AllKeysLrm,
    VolatileLrm,
}

impl Policy {
    pub fn name(self) -> &'static str {
        match self {
            Policy::NoEviction => "noeviction",
            Policy::AllKeysLru => "allkeys-lru",
            Policy::AllKeysLfu => "allkeys-lfu",
            Policy::AllKeysRandom => "allkeys-random",
            Policy::VolatileLru => "volatile-lru",
            Policy::VolatileLfu => "volatile-lfu",
            Policy::VolatileRandom => "volatile-random",
            Policy::VolatileTtl => "volatile-ttl",
            Policy::AllKeysLrm => "allkeys-lrm",
            Policy::VolatileLrm => "volatile-lrm",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "noeviction" => Policy::NoEviction,
            "allkeys-lru" => Policy::AllKeysLru,
            "allkeys-lfu" => Policy::AllKeysLfu,
            "allkeys-random" => Policy::AllKeysRandom,
            "volatile-lru" => Policy::VolatileLru,
            "volatile-lfu" => Policy::VolatileLfu,
            "volatile-random" => Policy::VolatileRandom,
            "volatile-ttl" => Policy::VolatileTtl,
            "allkeys-lrm" => Policy::AllKeysLrm,
            "volatile-lrm" => Policy::VolatileLrm,
            _ => return None,
        })
    }

    pub fn is_lfu(self) -> bool {
        matches!(self, Policy::AllKeysLfu | Policy::VolatileLfu)
    }

    pub fn volatile_only(self) -> bool {
        matches!(self, Policy::VolatileLru | Policy::VolatileLfu | Policy::VolatileRandom | Policy::VolatileTtl | Policy::VolatileLrm)
    }

    /// The LRU clock tracks writes only (least recently modified).
    pub fn is_lrm(self) -> bool {
        matches!(self, Policy::AllKeysLrm | Policy::VolatileLrm)
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// 0 = no limit.
    pub maxmemory: u64,
    /// Highest `maxmemory` a tenant may set (the plan's memory); 0 = no cap.
    pub maxmemory_cap: u64,
    pub maxmemory_policy: Policy,
    pub maxmemory_samples: u32,
    pub maxmemory_eviction_tenacity: u32,
    pub lfu_log_factor: u32,
    pub lfu_decay_time: u32,
    /// Bitmask of `notify::*` classes.
    pub notify: u32,
    pub hash_max_listpack_entries: usize,
    pub hash_max_listpack_value: usize,
    pub set_max_intset_entries: usize,
    pub set_max_listpack_entries: usize,
    pub set_max_listpack_value: usize,
    pub zset_max_listpack_entries: usize,
    pub zset_max_listpack_value: usize,
    pub list_max_listpack_size: i64,
    pub list_compress_depth: u32,
    pub stream_node_max_bytes: usize,
    pub stream_node_max_entries: usize,
    /// Default IDMP settings of new streams (seconds / entries per producer).
    pub stream_idmp_duration: u64,
    pub stream_idmp_maxsize: u64,
    pub hll_sparse_max_bytes: usize,
    /// Arrays: slice size of new arrays (a power of two), and the sparse-slice thresholds.
    pub array_slice_size: u32,
    pub array_sparse_kmax: u32,
    pub array_sparse_kmin: u32,
    /// Lua/function busy threshold in milliseconds (`busy-reply-threshold`, alias `lua-time-limit`).
    pub busy_reply_threshold: u64,
    pub proto_max_bulk_len: u64,
    pub slowlog_log_slower_than: i64,
    pub slowlog_max_len: u64,
    pub active_expire_effort: u32,
    pub latency_monitor_threshold: u64,
    pub latency_tracking: bool,
    /// `latency-tracking-info-percentiles`.
    pub latency_percentiles: Vec<f64>,
    pub tracking_table_max_keys: u64,
    pub maxclients: u64,
    /// Client idle timeout in seconds (0 = never).
    pub timeout: u64,
    pub lazyfree_lazy_eviction: bool,
    pub lazyfree_lazy_expire: bool,
    pub lazyfree_lazy_server_del: bool,
    pub lazyfree_lazy_user_del: bool,
    pub lazyfree_lazy_user_flush: bool,
    pub replica_lazy_flush: bool,
    pub client_query_buffer_limit: u64,
    pub databases: usize,
    pub hz: u32,
    pub enable_debug_command: bool,
    /// Microseconds a background save spends per key (a hidden testing knob).
    pub rdb_key_save_delay: i64,
    pub key_load_delay: i64,
    /// Credentials for a Redis primary followed with REPLICAOF.
    pub masterauth: String,
    pub masteruser: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            maxmemory: 0,
            maxmemory_cap: 0,
            maxmemory_policy: Policy::NoEviction,
            maxmemory_samples: 5,
            maxmemory_eviction_tenacity: 10,
            lfu_log_factor: 10,
            lfu_decay_time: 1,
            notify: 0,
            hash_max_listpack_entries: 512,
            hash_max_listpack_value: 64,
            set_max_intset_entries: 512,
            set_max_listpack_entries: 128,
            set_max_listpack_value: 64,
            zset_max_listpack_entries: 128,
            zset_max_listpack_value: 64,
            list_max_listpack_size: -2,
            list_compress_depth: 0,
            stream_node_max_bytes: 4096,
            stream_node_max_entries: 100,
            stream_idmp_duration: 100,
            stream_idmp_maxsize: 100,
            hll_sparse_max_bytes: 3000,
            array_slice_size: crate::types::array::SLICE_SIZE_DEFAULT,
            array_sparse_kmax: crate::types::array::KMAX_DEFAULT,
            array_sparse_kmin: crate::types::array::KMIN_DEFAULT,
            busy_reply_threshold: 5000,
            proto_max_bulk_len: 512 * 1024 * 1024,
            slowlog_log_slower_than: 10_000,
            slowlog_max_len: 128,
            active_expire_effort: 1,
            latency_monitor_threshold: 0,
            latency_tracking: true,
            latency_percentiles: vec![50.0, 99.0, 99.9],
            tracking_table_max_keys: 1_000_000,
            maxclients: 10_000,
            timeout: 0,
            lazyfree_lazy_eviction: false,
            lazyfree_lazy_expire: false,
            lazyfree_lazy_server_del: false,
            lazyfree_lazy_user_del: false,
            lazyfree_lazy_user_flush: false,
            replica_lazy_flush: false,
            client_query_buffer_limit: 1024 * 1024 * 1024,
            databases: 16,
            hz: 10,
            enable_debug_command: true,
            rdb_key_save_delay: 0,
            key_load_delay: 0,
            masterauth: String::new(),
            masteruser: String::new(),
        }
    }
}

type Getter = fn(&Config) -> String;
type Setter = fn(&mut Config, &str) -> Result<(), String>;

pub struct Param {
    pub name: &'static str,
    pub alias: Option<&'static str>,
    pub get: Getter,
    /// `None` = immutable (managed by the platform).
    pub set: Option<Setter>,
}

fn yesno(b: bool) -> String {
    if b { "yes".into() } else { "no".into() }
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err("argument must be 'yes' or 'no'".into()),
    }
}

/// `memtoull`: `100`, `1k`, `1kb`, `5mb`, `2gb` (k = 1000, kb = 1024).
pub fn parse_memory(v: &str) -> Result<u64, String> {
    let l = v.to_ascii_lowercase();
    let (num, mul) = [("gb", 1u64 << 30), ("mb", 1 << 20), ("kb", 1 << 10), ("g", 1_000_000_000), ("m", 1_000_000), ("k", 1000), ("b", 1)]
        .iter()
        .find_map(|(suf, m)| l.strip_suffix(suf).map(|n| (n, *m)))
        .unwrap_or((l.as_str(), 1));
    let n: u64 = num.parse().map_err(|_| "argument must be a memory value".to_string())?;
    n.checked_mul(mul).ok_or_else(|| "argument must be a memory value".to_string())
}

fn parse_num<T: std::str::FromStr>(v: &str) -> Result<T, String> {
    v.parse().map_err(|_| "argument couldn't be parsed into an integer".to_string())
}

/// An unsigned integer config within `[min, max]`.
fn ranged_u32(v: &str, min: u32, max: u32) -> Result<u32, String> {
    let range = || format!("argument must be between {min} and {max} inclusive");
    if v.parse::<i128>().is_ok_and(|w| w < min as i128 || w > max as i128) {
        return Err(range());
    }
    let n: u32 = parse_num(v)?;
    if n < min || n > max {
        return Err(range());
    }
    Ok(n)
}

macro_rules! p {
    ($name:literal, $field:ident, bool) => {
        Param { name: $name, alias: None, get: |c| yesno(c.$field), set: Some(|c, v| { c.$field = parse_bool(v)?; Ok(()) }) }
    };
    ($name:literal, $field:ident, num) => {
        Param { name: $name, alias: None, get: |c| c.$field.to_string(), set: Some(|c, v| { c.$field = parse_num(v)?; Ok(()) }) }
    };
    ($name:literal, $field:ident, num, $min:expr, $max:expr) => {
        Param {
            name: $name,
            alias: None,
            get: |c| c.$field.to_string(),
            set: Some(|c, v| {
                // An integer of the wrong sign is out of range rather than unparsable.
                if v.parse::<i128>().is_ok_and(|w| w < $min as i128 || w > $max as i128) {
                    return Err(format!("argument must be between {} and {} inclusive", $min, $max));
                }
                let n = parse_num(v)?;
                #[allow(unused_comparisons, clippy::manual_range_contains)]
                let out_of_range = n < $min || n > $max;
                if out_of_range {
                    return Err(format!("argument must be between {} and {} inclusive", $min, $max));
                }
                c.$field = n;
                Ok(())
            }),
        }
    };
    ($name:literal, $value:literal) => {
        Param { name: $name, alias: None, get: |_| $value.to_string(), set: None }
    };
}

pub static PARAMS: &[Param] = &[
    Param {
        name: "maxmemory",
        alias: None,
        get: |c| c.maxmemory.to_string(),
        set: Some(|c, v| {
            let m = parse_memory(v)?;
            // Hosted databases can lower their limit but not exceed their plan.
            if c.maxmemory_cap > 0 && (m == 0 || m > c.maxmemory_cap) {
                return Err(format!("maxmemory can't exceed this database's plan limit of {} bytes", c.maxmemory_cap));
            }
            c.maxmemory = m;
            Ok(())
        }),
    },
    Param {
        name: "maxmemory-policy",
        alias: None,
        get: |c| c.maxmemory_policy.name().into(),
        set: Some(|c, v| {
            c.maxmemory_policy = Policy::parse(v).ok_or("argument(s) must be one of the following: volatile-lru, volatile-lfu, volatile-random, volatile-ttl, volatile-lrm, allkeys-lru, allkeys-lfu, allkeys-random, allkeys-lrm, noeviction")?;
            Ok(())
        }),
    },
    p!("maxmemory-samples", maxmemory_samples, num, 1u32, 64u32),
    p!("maxmemory-eviction-tenacity", maxmemory_eviction_tenacity, num, 0u32, 100u32),
    p!("lfu-log-factor", lfu_log_factor, num, 0u32, i32::MAX as u32),
    p!("lfu-decay-time", lfu_decay_time, num, 0u32, i32::MAX as u32),
    Param {
        name: "notify-keyspace-events",
        alias: None,
        get: |c| notify::flags_to_string(c.notify),
        set: Some(|c, v| {
            c.notify = notify::parse_flags(v).ok_or("Invalid event class character. Use 'Ag$lshzxeKEtmdn'.")?;
            Ok(())
        }),
    },
    p!("hash-max-listpack-entries", hash_max_listpack_entries, num),
    Param { name: "hash-max-ziplist-entries", alias: None, get: |c| c.hash_max_listpack_entries.to_string(), set: Some(|c, v| { c.hash_max_listpack_entries = parse_num(v)?; Ok(()) }) },
    p!("hash-max-listpack-value", hash_max_listpack_value, num),
    Param { name: "hash-max-ziplist-value", alias: None, get: |c| c.hash_max_listpack_value.to_string(), set: Some(|c, v| { c.hash_max_listpack_value = parse_num(v)?; Ok(()) }) },
    p!("set-max-intset-entries", set_max_intset_entries, num),
    p!("set-max-listpack-entries", set_max_listpack_entries, num),
    p!("set-max-listpack-value", set_max_listpack_value, num),
    p!("zset-max-listpack-entries", zset_max_listpack_entries, num),
    Param { name: "zset-max-ziplist-entries", alias: None, get: |c| c.zset_max_listpack_entries.to_string(), set: Some(|c, v| { c.zset_max_listpack_entries = parse_num(v)?; Ok(()) }) },
    p!("zset-max-listpack-value", zset_max_listpack_value, num),
    Param { name: "zset-max-ziplist-value", alias: None, get: |c| c.zset_max_listpack_value.to_string(), set: Some(|c, v| { c.zset_max_listpack_value = parse_num(v)?; Ok(()) }) },
    p!("list-max-listpack-size", list_max_listpack_size, num, i32::MIN as i64, i32::MAX as i64),
    Param { name: "list-max-ziplist-size", alias: None, get: |c| c.list_max_listpack_size.to_string(), set: Some(|c, v| { c.list_max_listpack_size = parse_num(v)?; Ok(()) }) },
    p!("list-compress-depth", list_compress_depth, num),
    p!("stream-node-max-bytes", stream_node_max_bytes, num),
    p!("stream-node-max-entries", stream_node_max_entries, num),
    p!("stream-idmp-duration", stream_idmp_duration, num, 1u64, 86400u64),
    p!("stream-idmp-maxsize", stream_idmp_maxsize, num, 1u64, 10000u64),
    p!("hll-sparse-max-bytes", hll_sparse_max_bytes, num),
    Param {
        name: "array-slice-size",
        alias: None,
        get: |c| c.array_slice_size.to_string(),
        set: Some(|c, v| {
            let n = ranged_u32(v, 256, 65536)?;
            if !n.is_power_of_two() {
                return Err("array-slice-size must be a power of two".into());
            }
            c.array_slice_size = n;
            Ok(())
        }),
    },
    Param {
        name: "array-sparse-kmax",
        alias: None,
        get: |c| c.array_sparse_kmax.to_string(),
        set: Some(|c, v| {
            let n = ranged_u32(v, 0, 256)?;
            if n > 0 && n <= c.array_sparse_kmin {
                return Err("array-sparse-kmax must be greater than array-sparse-kmin when non-zero".into());
            }
            c.array_sparse_kmax = n;
            Ok(())
        }),
    },
    Param {
        name: "array-sparse-kmin",
        alias: None,
        get: |c| c.array_sparse_kmin.to_string(),
        set: Some(|c, v| {
            let n = ranged_u32(v, 0, 256)?;
            if c.array_sparse_kmax > 0 && n >= c.array_sparse_kmax {
                return Err("array-sparse-kmin must be less than array-sparse-kmax".into());
            }
            c.array_sparse_kmin = n;
            Ok(())
        }),
    },
    Param {
        name: "busy-reply-threshold",
        alias: Some("lua-time-limit"),
        get: |c| c.busy_reply_threshold.to_string(),
        set: Some(|c, v| {
            c.busy_reply_threshold = parse_num(v)?;
            Ok(())
        }),
    },
    Param {
        name: "proto-max-bulk-len",
        alias: None,
        get: |c| c.proto_max_bulk_len.to_string(),
        set: Some(|c, v| {
            let n = parse_memory(v)?;
            if n < 1024 * 1024 {
                return Err("argument must be between 1048576 and 9223372036854775807 inclusive".into());
            }
            c.proto_max_bulk_len = n;
            Ok(())
        }),
    },
    p!("slowlog-log-slower-than", slowlog_log_slower_than, num),
    p!("slowlog-max-len", slowlog_max_len, num),
    p!("active-expire-effort", active_expire_effort, num, 1u32, 10u32),
    p!("latency-monitor-threshold", latency_monitor_threshold, num),
    p!("latency-tracking", latency_tracking, bool),
    Param {
        name: "latency-tracking-info-percentiles",
        alias: None,
        get: |c| c.latency_percentiles.iter().map(|p| crate::commands::info::trim_double(*p)).collect::<Vec<_>>().join(" "),
        set: Some(|c, v| {
            let mut out = Vec::new();
            for a in v.split_whitespace() {
                let p: f64 = a.parse().ok().filter(|p: &f64| p.is_finite()).ok_or("Invalid latency-tracking-info-percentiles parameters")?;
                if !(0.0..=100.0).contains(&p) {
                    return Err("latency-tracking-info-percentiles parameters should sit between [0.0,100.0]".into());
                }
                out.push(p);
            }
            c.latency_percentiles = out;
            Ok(())
        }),
    },
    p!("tracking-table-max-keys", tracking_table_max_keys, num),
    p!("maxclients", maxclients, num, 1u64, u32::MAX as u64),
    p!("timeout", timeout, num),
    p!("lazyfree-lazy-eviction", lazyfree_lazy_eviction, bool),
    p!("lazyfree-lazy-expire", lazyfree_lazy_expire, bool),
    p!("lazyfree-lazy-server-del", lazyfree_lazy_server_del, bool),
    p!("lazyfree-lazy-user-del", lazyfree_lazy_user_del, bool),
    p!("lazyfree-lazy-user-flush", lazyfree_lazy_user_flush, bool),
    p!("replica-lazy-flush", replica_lazy_flush, bool),
    Param {
        name: "client-query-buffer-limit",
        alias: None,
        get: |c| c.client_query_buffer_limit.to_string(),
        set: Some(|c, v| {
            c.client_query_buffer_limit = parse_memory(v)?;
            Ok(())
        }),
    },
    p!("hz", hz, num, 1u32, 500u32),
    Param { name: "databases", alias: None, get: |c| c.databases.to_string(), set: None },
    // Platform-managed: visible for compatibility, not settable by tenants.
    p!("appendonly", "yes"),
    p!("appendfsync", "everysec"),
    p!("save", ""),
    p!("dir", "/data"),
    p!("dbfilename", "dump.rdb"),
    p!("appendfilename", "appendonly.aof"),
    p!("port", "6379"),
    p!("bind", "* -::*"),
    p!("protected-mode", "no"),
    p!("daemonize", "no"),
    p!("io-threads", "1"),
    p!("cluster-enabled", "no"),
    p!("replica-read-only", "yes"),
    p!("replica-serve-stale-data", "yes"),
    p!("repl-diskless-sync", "yes"),
    p!("loglevel", "notice"),
    p!("tcp-keepalive", "300"),
    p!("tcp-backlog", "511"),
    p!("rdbcompression", "yes"),
    p!("rdbchecksum", "yes"),
    p!("activerehashing", "yes"),
    p!("activedefrag", "no"),
    p!("jemalloc-bg-thread", "yes"),
    p!("aof-use-rdb-preamble", "yes"),
    p!("lua-replicate-commands", "yes"),
    p!("enable-protected-configs", "no"),
    p!("enable-module-command", "no"),
    p!("min-replicas-to-write", "0"),
    p!("min-replicas-max-lag", "10"),
    p!("client-output-buffer-limit", "normal 0 0 0 slave 268435456 67108864 60 pubsub 33554432 8388608 60"),
    p!("maxmemory-clients", "0"),
    p!("hash-max-template-entries", "0"),
    p!("hash-rdb-load-max-template-entries", "0"),
    p!("hide-user-data-from-log", "no"),
    p!("requirepass", ""),
    Param { name: "masterauth", alias: None, get: |c| c.masterauth.clone(), set: Some(|c, v| { c.masterauth = v.to_string(); Ok(()) }) },
    Param { name: "masteruser", alias: None, get: |c| c.masteruser.clone(), set: Some(|c, v| { c.masteruser = v.to_string(); Ok(()) }) },
    // Hidden: only an exact CONFIG GET name finds these.
    p!("rdb-key-save-delay", rdb_key_save_delay, num, i32::MIN as i64, i32::MAX as i64),
    p!("key-load-delay", key_load_delay, num, i32::MIN as i64, i32::MAX as i64),
    p!("repl-rdb-channel", "yes"),
    p!("use-exit-on-panic", "no"),
    p!("aof-disable-auto-gc", "no"),
    p!("lazyexpire-nested-arbitrary-keys", "yes"),
    p!("lua-enable-deprecated-api", "no"),
    p!("prefetch-batch-max-size", "16"),
    p!("watchdog-period", "0"),
    p!("cluster-slot-migration-max-archived-tasks", "32"),
    p!("cluster-ping-interval", "0"),
    p!("cluster-slot-migration-sync-buffer-drain-timeout", "60000"),
    p!("loading-process-events-interval-bytes", "524288"),
];

/// Redis' HIDDEN_CONFIG parameters: left out of pattern matches.
const HIDDEN: &[&str] = &[
    "rdb-key-save-delay",
    "key-load-delay",
    "repl-rdb-channel",
    "use-exit-on-panic",
    "aof-disable-auto-gc",
    "lazyexpire-nested-arbitrary-keys",
    "lua-enable-deprecated-api",
    "prefetch-batch-max-size",
    "watchdog-period",
    "cluster-slot-migration-max-archived-tasks",
    "cluster-ping-interval",
    "cluster-slot-migration-sync-buffer-drain-timeout",
    "loading-process-events-interval-bytes",
];

pub fn find(name: &str) -> Option<&'static Param> {
    PARAMS.iter().find(|p| p.name.eq_ignore_ascii_case(name) || p.alias.is_some_and(|a| a.eq_ignore_ascii_case(name)))
}

impl Config {
    /// `CONFIG GET pattern`: matching `(name, value)` pairs, aliases included like Redis. A name
    /// without glob characters is looked up directly (hidden parameters too) and echoed as given.
    pub fn get(&self, pattern: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if !pattern.iter().any(|c| b"[*?".contains(c)) {
            let name = String::from_utf8_lossy(pattern);
            if let Some(p) = find(&name) {
                out.push((name.into_owned(), (p.get)(self)));
            }
            return out;
        }
        let mut seen = std::collections::HashSet::new();
        for p in PARAMS.iter().filter(|p| !HIDDEN.contains(&p.name)) {
            for name in std::iter::once(p.name).chain(p.alias) {
                if crate::util::glob_match(pattern, name.as_bytes(), true) && seen.insert(name) {
                    out.push((name.to_string(), (p.get)(self)));
                }
            }
        }
        out
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let p = find(name).ok_or_else(|| format!("Unknown option or number of arguments for CONFIG SET - '{name}'"))?;
        let Some(set) = p.set else {
            return Err(format!("CONFIG SET failed (possibly related to argument '{name}') - can't set immutable config"));
        };
        set(self, value).map_err(|e| format!("CONFIG SET failed (possibly related to argument '{}') - {e}", name.to_ascii_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_and_set() {
        let mut c = Config::default();
        c.set("maxmemory", "100mb").unwrap();
        assert_eq!(c.maxmemory, 100 << 20);
        c.set("MAXMEMORY-POLICY", "allkeys-lru").unwrap();
        assert_eq!(c.get(b"maxmemory-policy"), vec![("maxmemory-policy".into(), "allkeys-lru".into())]);
        c.set("lua-time-limit", "100").unwrap();
        assert_eq!(c.busy_reply_threshold, 100);
        assert!(c.set("port", "1").is_err());
        assert!(c.set("nope", "1").is_err());
        assert!(c.set("maxmemory-samples", "0").is_err());
        assert!(c.get(b"*max-*-entries*").len() >= 4);
    }
}
