//! Runtime configuration (`CONFIG GET` / `CONFIG SET`), per database.
//!
//! Every parameter Redis 8.10 has is here with its type, bounds, flags, aliases and validation,
//! so CONFIG behaves like Redis' table-driven config. Parameters the engine acts on live in typed
//! fields; the rest are kept as values (`extra`) so they read back what was set. A few apply
//! steps fail like they would on a server without the matching facility (TLS, rebinding).

use std::collections::BTreeMap;

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

/// `maxmemory_policy_enum`, in Redis' order.
const POLICIES: &[(&str, i64)] = &[
    ("volatile-lru", 0),
    ("volatile-lfu", 1),
    ("volatile-random", 2),
    ("volatile-ttl", 3),
    ("volatile-lrm", 4),
    ("allkeys-lru", 5),
    ("allkeys-lfu", 6),
    ("allkeys-random", 7),
    ("allkeys-lrm", 8),
    ("noeviction", 9),
];

impl Policy {
    const ALL: [Policy; 10] = [
        Policy::VolatileLru,
        Policy::VolatileLfu,
        Policy::VolatileRandom,
        Policy::VolatileTtl,
        Policy::VolatileLrm,
        Policy::AllKeysLru,
        Policy::AllKeysLfu,
        Policy::AllKeysRandom,
        Policy::AllKeysLrm,
        Policy::NoEviction,
    ];

    fn index(self) -> i64 {
        Self::ALL.iter().position(|p| *p == self).unwrap() as i64
    }

    pub fn name(self) -> &'static str {
        POLICIES[self.index() as usize].0
    }

    pub fn parse(s: &str) -> Option<Self> {
        POLICIES.iter().position(|(n, _)| n.eq_ignore_ascii_case(s)).map(|i| Self::ALL[i])
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

/// `enable-debug-command` and friends: `no`, `yes` or `local` (loopback connections only).
pub const PROTECTED_NO: u8 = 0;
pub const PROTECTED_YES: u8 = 1;
pub const PROTECTED_LOCAL: u8 = 2;

/// Output buffer limits for one client class: hard bytes, soft bytes, soft seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObufLimit {
    pub hard: u64,
    pub soft: u64,
    pub soft_seconds: i64,
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
    /// Hash templates: field counts that make hashes share a field layout.
    pub hash_min_template_entries: usize,
    pub hash_max_template_entries: usize,
    pub hash_rdb_load_min_template_entries: usize,
    pub hash_rdb_load_max_template_entries: usize,
    pub hash_rdb_load_template_disassembly_threshold: usize,
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
    pub slowlog_max_argc: usize,
    pub slowlog_max_string_len: usize,
    pub acllog_max_len: u64,
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
    /// normal, replica, pubsub.
    pub obuf_limits: [ObufLimit; 3],
    pub databases: usize,
    pub hz: u32,
    pub enable_debug_command: u8,
    pub enable_module_command: u8,
    pub enable_protected_configs: u8,
    pub appendonly: bool,
    /// `save`: (seconds, changes) rules.
    pub save: Vec<(i64, i32)>,
    /// Microseconds a background save spends per key (a hidden testing knob).
    pub rdb_key_save_delay: i64,
    pub key_load_delay: i64,
    /// Credentials for a Redis primary followed with REPLICAOF.
    pub masterauth: String,
    pub masteruser: String,
    /// `requirepass`, as last set (the default user's password follows it).
    pub requirepass: Option<String>,
    /// Parameters without a typed field, by name; absent = default.
    pub extra: BTreeMap<&'static str, Val>,
}

impl Default for Config {
    fn default() -> Self {
        let n = ObufLimit { hard: 0, soft: 0, soft_seconds: 0 };
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
            hash_min_template_entries: 0,
            hash_max_template_entries: 0,
            hash_rdb_load_min_template_entries: 0,
            hash_rdb_load_max_template_entries: 0,
            hash_rdb_load_template_disassembly_threshold: 0,
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
            slowlog_max_argc: 32,
            slowlog_max_string_len: 128,
            acllog_max_len: 128,
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
            obuf_limits: [
                n,
                ObufLimit { hard: 256 << 20, soft: 64 << 20, soft_seconds: 60 },
                ObufLimit { hard: 32 << 20, soft: 8 << 20, soft_seconds: 60 },
            ],
            databases: 16,
            hz: 10,
            enable_debug_command: PROTECTED_NO,
            enable_module_command: PROTECTED_NO,
            enable_protected_configs: PROTECTED_NO,
            appendonly: false,
            save: vec![(3600, 1), (300, 100), (60, 10000)],
            rdb_key_save_delay: 0,
            key_load_delay: 0,
            masterauth: String::new(),
            masteruser: String::new(),
            requirepass: None,
            extra: BTreeMap::new(),
        }
    }
}

// ---- the table ----

/// Parameter flags (Redis' `*_CONFIG`).
pub const IMMUTABLE: u16 = 1 << 0;
pub const SENSITIVE: u16 = 1 << 1;
pub const HIDDEN: u16 = 1 << 2;
pub const PROTECTED: u16 = 1 << 3;
pub const DENY_LOADING: u16 = 1 << 4;
pub const MULTI_ARG: u16 = 1 << 5;
pub const VOLATILE: u16 = 1 << 6;
const MODIFIABLE: u16 = 0;
const DEBUG: u16 = 0;

/// Numeric parse/print flags.
const INTEGER: u8 = 0;
const MEMORY: u8 = 1;
const PERCENT: u8 = 2;
const OCTAL: u8 = 4;

const INT_MAX: i128 = i32::MAX as i128;
const INT_MIN: i128 = i32::MIN as i128;
const UINT_MAX: i128 = u32::MAX as i128;
const LONG_MAX: i128 = i64::MAX as i128;
const ULLONG_MAX: i128 = u64::MAX as i128;

/// A parameter's value.
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Bool(bool),
    /// `None`: unset (an empty string for `EMPTY_STRING_IS_NULL` strings).
    Str(Option<String>),
    /// Numbers and enum values.
    Num(i128),
}

impl Val {
    fn num(&self) -> i128 {
        match self {
            Val::Num(n) => *n,
            Val::Bool(b) => *b as i128,
            Val::Str(_) => 0,
        }
    }

    fn bool(&self) -> bool {
        self.num() != 0
    }

    fn string(self) -> Option<String> {
        match self {
            Val::Str(s) => s,
            _ => None,
        }
    }
}

type Get = fn(&Config) -> Val;
type Put = fn(&mut Config, Val);
type Check = fn(&Config, &Val) -> Result<(), String>;
type Apply = fn(&mut Config) -> Result<(), String>;
type SpecialGet = fn(&Config) -> String;
/// Returns whether the value changed.
type SpecialSet = fn(&mut Config, &[&str]) -> Result<bool, String>;

pub enum Ty {
    Bool(bool),
    Str { default: Option<&'static str>, null_empty: bool },
    Enum { values: &'static [(&'static str, i64)], default: i64 },
    Num { min: i128, max: i128, default: i128, unsigned: bool, nf: u8 },
    Special { get: SpecialGet, set: SpecialSet },
}

pub struct Param {
    pub name: &'static str,
    pub alias: Option<&'static str>,
    pub flags: u16,
    pub ty: Ty,
    field: Option<(Get, Put)>,
    check: Option<Check>,
    apply: Option<Apply>,
}

impl Param {
    const fn new(name: &'static str, alias: Option<&'static str>, flags: u16, ty: Ty) -> Self {
        Param { name, alias, flags, ty, field: None, check: None, apply: None }
    }

    const fn at(mut self, get: Get, put: Put) -> Self {
        self.field = Some((get, put));
        self
    }

    const fn check(mut self, f: Check) -> Self {
        self.check = Some(f);
        self
    }

    const fn apply(mut self, f: Apply) -> Self {
        self.apply = Some(f);
        self
    }

    fn default(&self) -> Val {
        match &self.ty {
            Ty::Bool(d) => Val::Bool(*d),
            Ty::Str { default, .. } => Val::Str(default.map(str::to_string)),
            Ty::Enum { default, .. } => Val::Num(*default as i128),
            Ty::Num { default, .. } => Val::Num(*default),
            Ty::Special { .. } => Val::Str(None),
        }
    }

    fn value(&self, c: &Config) -> Val {
        match self.field {
            Some((get, _)) => get(c),
            None => c.extra.get(self.name).cloned().unwrap_or_else(|| self.default()),
        }
    }

    fn store(&self, c: &mut Config, v: Val) {
        match self.field {
            Some((_, put)) => put(c, v),
            None => {
                c.extra.insert(self.name, v);
            }
        }
    }

    /// `interface.get`.
    pub fn get(&self, c: &Config) -> String {
        match &self.ty {
            Ty::Special { get, .. } => get(c),
            Ty::Bool(_) => yesno(self.value(c).bool()).into(),
            Ty::Str { .. } => self.value(c).string().unwrap_or_default(),
            Ty::Enum { values, .. } => enum_name(values, self.value(c).num() as i64, self.flags & MULTI_ARG != 0),
            Ty::Num { nf, .. } => {
                let v = self.value(c).num();
                if nf & PERCENT != 0 && v < 0 {
                    format!("{}%", -v)
                } else if nf & OCTAL != 0 {
                    format!("{v:o}")
                } else {
                    v.to_string()
                }
            }
        }
    }

    /// `performInterfaceSet`: parse and validate `value`, store it. Returns whether it changed.
    fn set(&self, c: &mut Config, value: &str) -> Result<bool, String> {
        let argv: Vec<&str> = if self.flags & MULTI_ARG == 0 {
            vec![value]
        } else if value.is_empty() {
            vec![]
        } else {
            value.split(' ').collect()
        };
        let new = match &self.ty {
            Ty::Special { set, .. } => return set(c, &argv),
            Ty::Bool(_) => Val::Bool(match value.to_ascii_lowercase().as_str() {
                "yes" => true,
                "no" => false,
                _ => return Err("argument must be 'yes' or 'no'".into()),
            }),
            Ty::Str { null_empty, .. } => {
                // Values are C strings.
                let s = value.split('\0').next().unwrap_or("");
                Val::Str(if *null_empty && s.is_empty() { None } else { Some(s.to_string()) })
            }
            Ty::Enum { values, .. } => {
                let multi = self.flags & MULTI_ARG != 0;
                Val::Num(enum_value(values, &argv, multi).ok_or_else(|| {
                    format!("argument(s) must be one of the following: {}", values.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "))
                })? as i128)
            }
            Ty::Num { min, max, unsigned, nf, .. } => {
                let ll = parse_numeric(value, *nf)?;
                Val::Num(bound_check(ll, *min, *max, *unsigned, *nf)?)
            }
        };
        if let Some(check) = self.check {
            check(c, &new)?;
        }
        let changed = self.value(c) != new;
        self.store(c, new);
        Ok(changed || self.flags & VOLATILE != 0)
    }
}

fn yesno(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// `configEnumGetValue`.
fn enum_value(values: &[(&str, i64)], argv: &[&str], multi: bool) -> Option<i64> {
    if argv.is_empty() || (!multi && argv.len() != 1) {
        return None;
    }
    let mut v = 0;
    for a in argv {
        let mut matched = false;
        for (n, x) in values {
            if n.eq_ignore_ascii_case(a) {
                v |= x;
                matched = true;
            }
        }
        if !matched {
            return None;
        }
    }
    Some(v)
}

/// `configEnumGetName`.
fn enum_name(values: &[(&str, i64)], v: i64, multi: bool) -> String {
    let mut names: Vec<&str> = Vec::new();
    let mut unmatched = v;
    for (n, x) in values {
        if v == *x {
            return n.to_string();
        }
        if multi && *x != 0 && *x == unmatched & x {
            names.push(n);
            unmatched &= !x;
        }
    }
    if names.is_empty() || unmatched != 0 { "unknown".into() } else { names.join(" ") }
}

/// `memtoull`: `100`, `1k`, `1kb`, `5mb`, `2gb` (k = 1000, kb = 1024), case-insensitive units.
pub fn memtoull(s: &str) -> Option<u64> {
    if s.starts_with('-') {
        return None;
    }
    let split = s.bytes().position(|c| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, unit) = s.split_at(split);
    let mul: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" => 1000,
        "kb" => 1024,
        "m" => 1000 * 1000,
        "mb" => 1024 * 1024,
        "g" => 1000 * 1000 * 1000,
        "gb" => 1024 * 1024 * 1024,
        _ => return None,
    };
    if digits.is_empty() || digits.len() >= 128 {
        return None;
    }
    let val = digits.parse::<u64>().unwrap_or(u64::MAX);
    Some(if val > u64::MAX / mul { u64::MAX } else { val * mul })
}

/// `memtoull` with Redis' error text, for callers outside the table.
pub fn parse_memory(v: &str) -> Result<u64, String> {
    memtoull(v).ok_or_else(|| "argument must be a memory value".to_string())
}

/// `numericParseString`: the value as C `long long`.
fn parse_numeric(value: &str, nf: u8) -> Result<i64, String> {
    if nf & MEMORY != 0
        && let Some(v) = memtoull(value)
    {
        return Ok(v as i64);
    }
    if nf & PERCENT != 0
        && value.len() > 1
        && let Some(p) = value.strip_suffix('%')
        && let Some(n) = crate::util::parse_i64(p.as_bytes())
        && n >= 0
    {
        return Ok(-n);
    }
    if nf & OCTAL != 0
        && let Some(v) = strtoll_radix8(value)
    {
        return Ok(v);
    }
    if nf == INTEGER
        && let Some(v) = crate::util::parse_i64(value.as_bytes())
    {
        return Ok(v);
    }
    Err(if nf & MEMORY != 0 && nf & PERCENT != 0 {
        "argument must be a memory or percent value"
    } else if nf & MEMORY != 0 {
        "argument must be a memory value"
    } else if nf & OCTAL != 0 {
        "argument couldn't be parsed as an octal number"
    } else {
        "argument couldn't be parsed into an integer"
    }
    .into())
}

fn strtoll_radix8(s: &str) -> Option<i64> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (neg, d) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if s.is_empty() {
        return Some(0);
    }
    if d.is_empty() || !d.bytes().all(|c| (b'0'..=b'7').contains(&c)) {
        return None;
    }
    let v = i64::from_str_radix(d, 8).ok()?;
    Some(if neg { -v } else { v })
}

/// `numericBoundaryCheck`: the value to store.
fn bound_check(ll: i64, min: i128, max: i128, unsigned: bool, nf: u8) -> Result<i128, String> {
    if unsigned {
        let u = ll as u64 as i128;
        if u > max || u < min {
            return Err(if nf & OCTAL != 0 {
                format!("argument must be between {min:o} and {max:o} inclusive")
            } else {
                format!("argument must be between {min} and {max} inclusive")
            });
        }
        return Ok(u);
    }
    let v = ll as i128;
    if nf & PERCENT != 0 && v < 0 {
        if v < min {
            return Err(format!("percentage argument must be less or equal to {}", -min));
        }
    } else if v > max || v < min {
        return Err(format!("argument must be between {min} and {max} inclusive"));
    }
    Ok(v)
}

const fn boolean(name: &'static str, alias: Option<&'static str>, flags: u16, default: bool) -> Param {
    Param::new(name, alias, flags, Ty::Bool(default))
}

const fn string(name: &'static str, alias: Option<&'static str>, flags: u16, null_empty: bool, default: Option<&'static str>) -> Param {
    Param::new(name, alias, flags, Ty::Str { default, null_empty })
}

const fn enumeration(name: &'static str, alias: Option<&'static str>, flags: u16, values: &'static [(&'static str, i64)], default: i64) -> Param {
    Param::new(name, alias, flags, Ty::Enum { values, default })
}

/// A signed numeric (`int`, `long`, `long long`, `ssize_t`, `time_t`, `off_t`, and — as Redis
/// checks it — `unsigned long`).
#[allow(clippy::too_many_arguments)]
const fn signed(name: &'static str, alias: Option<&'static str>, flags: u16, min: i128, max: i128, default: i128, nf: u8) -> Param {
    Param::new(name, alias, flags, Ty::Num { min, max, default, unsigned: false, nf })
}

/// An unsigned numeric (`unsigned int`, `unsigned long long`, `size_t`).
const fn unsigned(name: &'static str, alias: Option<&'static str>, flags: u16, min: i128, max: i128, default: i128, nf: u8) -> Param {
    Param::new(name, alias, flags, Ty::Num { min, max, default, unsigned: true, nf })
}

const fn special(name: &'static str, alias: Option<&'static str>, flags: u16, get: SpecialGet, set: SpecialSet) -> Param {
    Param::new(name, alias, flags, Ty::Special { get, set })
}

macro_rules! b {
    ($f:ident) => {
        (|c: &Config| Val::Bool(c.$f), |c: &mut Config, v: Val| c.$f = v.bool())
    };
}

macro_rules! n {
    ($f:ident) => {
        (|c: &Config| Val::Num(c.$f as i128), |c: &mut Config, v: Val| c.$f = v.num() as _)
    };
}

macro_rules! at {
    ($p:expr, $acc:expr) => {{
        let (g, s) = $acc;
        $p.at(g, s)
    }};
}

const SYSLOG_FACILITY: &[(&str, i64)] =
    &[("user", 8), ("local0", 128), ("local1", 136), ("local2", 144), ("local3", 152), ("local4", 160), ("local5", 168), ("local6", 176), ("local7", 184)];
const LOGLEVEL: &[(&str, i64)] = &[("debug", 0), ("verbose", 1), ("notice", 2), ("warning", 3), ("nothing", 4)];
const SUPERVISED: &[(&str, i64)] = &[("upstart", 1), ("systemd", 2), ("auto", 3), ("no", 0)];
const AOF_FSYNC: &[(&str, i64)] = &[("everysec", 2), ("always", 1), ("no", 0)];
const SHUTDOWN_ON_SIG: &[(&str, i64)] = &[("default", 0), ("save", 1), ("nosave", 2), ("now", 4), ("force", 8)];
const CLUSTER_SLOT_STATS: &[(&str, i64)] = &[("no", 0), ("yes", 7), ("cpu", 1), ("net", 2), ("mem", 4)];
const REPL_DISKLESS_LOAD: &[(&str, i64)] = &[("disabled", 0), ("on-empty-db", 1), ("swapdb", 2), ("flushdb", 3)];
const TLS_AUTH_CLIENTS: &[(&str, i64)] = &[("no", 0), ("yes", 1), ("optional", 2)];
const TLS_CLIENT_AUTH_USER: &[(&str, i64)] = &[("CN", 1), ("off", 0)];
const OOM_SCORE_ADJ: &[(&str, i64)] = &[("no", 0), ("yes", 1), ("relative", 1), ("absolute", 2)];
const ACL_PUBSUB_DEFAULT: &[(&str, i64)] = &[("allchannels", 1 << 6), ("resetchannels", 0)];
const SANITIZE_DUMP: &[(&str, i64)] = &[("no", 0), ("yes", 1), ("clients", 2)];
const PROTECTED_ACTION: &[(&str, i64)] = &[("no", 0), ("yes", 1), ("local", 2)];
const ENDPOINT_TYPE: &[(&str, i64)] = &[("ip", 0), ("hostname", 1), ("unknown-endpoint", 2)];
const PROPAGATION_ERROR: &[(&str, i64)] = &[("ignore", 0), ("panic", 1), ("panic-on-replicas", 2)];

/// Redis' platform port and bind addresses (CONFIG reports them; the platform owns the listeners).
const PORT: i128 = 6379;
const BIND: &str = "* -::*";

pub static PARAMS: &[Param] = &[
    // Bool configs
    boolean("rdbchecksum", None, IMMUTABLE, true),
    boolean("daemonize", None, IMMUTABLE, false),
    boolean("always-show-logo", None, IMMUTABLE, false),
    boolean("protected-mode", None, MODIFIABLE, false),
    boolean("rdbcompression", None, MODIFIABLE, true),
    boolean("rdb-del-sync-files", None, MODIFIABLE, false),
    boolean("activerehashing", None, MODIFIABLE, true),
    boolean("stop-writes-on-bgsave-error", None, MODIFIABLE, true),
    boolean("set-proc-title", None, IMMUTABLE, true),
    boolean("dynamic-hz", None, MODIFIABLE, true),
    at!(boolean("lazyfree-lazy-eviction", None, DEBUG | MODIFIABLE, false), b!(lazyfree_lazy_eviction)),
    at!(boolean("lazyfree-lazy-expire", None, DEBUG | MODIFIABLE, false), b!(lazyfree_lazy_expire)),
    at!(boolean("lazyfree-lazy-server-del", None, DEBUG | MODIFIABLE, false), b!(lazyfree_lazy_server_del)),
    at!(boolean("lazyfree-lazy-user-del", None, DEBUG | MODIFIABLE, false), b!(lazyfree_lazy_user_del)),
    at!(boolean("lazyfree-lazy-user-flush", None, DEBUG | MODIFIABLE, false), b!(lazyfree_lazy_user_flush)),
    boolean("repl-disable-tcp-nodelay", None, MODIFIABLE, false),
    boolean("repl-diskless-sync", None, DEBUG | MODIFIABLE, true),
    boolean("repl-rdb-channel", None, MODIFIABLE | HIDDEN, true),
    boolean("aof-rewrite-incremental-fsync", None, MODIFIABLE, true),
    boolean("no-appendfsync-on-rewrite", None, MODIFIABLE, false),
    boolean("cluster-require-full-coverage", None, MODIFIABLE, true),
    boolean("rdb-save-incremental-fsync", None, MODIFIABLE, true),
    boolean("aof-load-truncated", None, MODIFIABLE, true),
    boolean("aof-use-rdb-preamble", None, MODIFIABLE, true),
    boolean("aof-timestamp-enabled", None, MODIFIABLE, false),
    boolean("cluster-replica-no-failover", Some("cluster-slave-no-failover"), MODIFIABLE, false),
    at!(boolean("replica-lazy-flush", Some("slave-lazy-flush"), MODIFIABLE, false), b!(replica_lazy_flush)),
    boolean("replica-serve-stale-data", Some("slave-serve-stale-data"), MODIFIABLE, true),
    boolean("replica-read-only", Some("slave-read-only"), DEBUG | MODIFIABLE, true),
    boolean("replica-ignore-maxmemory", Some("slave-ignore-maxmemory"), MODIFIABLE, true),
    boolean("jemalloc-bg-thread", None, MODIFIABLE, true),
    boolean("activedefrag", None, DEBUG | MODIFIABLE, false),
    boolean("syslog-enabled", None, IMMUTABLE, false),
    boolean("cluster-enabled", None, IMMUTABLE, false),
    boolean("cluster-bus-port-protected-mode", None, MODIFIABLE, false),
    at!(boolean("appendonly", None, MODIFIABLE, false), b!(appendonly)),
    boolean("cluster-allow-reads-when-down", None, MODIFIABLE, false),
    boolean("cluster-allow-pubsubshard-when-down", None, MODIFIABLE, true),
    boolean("crash-log-enabled", None, MODIFIABLE, true),
    boolean("crash-memcheck-enabled", None, MODIFIABLE, true),
    boolean("use-exit-on-panic", None, MODIFIABLE | HIDDEN, false),
    boolean("disable-thp", None, IMMUTABLE, true),
    boolean("cluster-allow-replica-migration", None, MODIFIABLE, true),
    boolean("replica-announced", None, MODIFIABLE, true),
    at!(boolean("latency-tracking", None, MODIFIABLE, true), b!(latency_tracking)),
    boolean("aof-disable-auto-gc", None, MODIFIABLE | HIDDEN, false),
    boolean("replica-ignore-disk-write-errors", None, MODIFIABLE, false),
    boolean("hide-user-data-from-log", None, MODIFIABLE, false),
    boolean("lazyexpire-nested-arbitrary-keys", None, MODIFIABLE | HIDDEN, true),
    enumeration("cluster-slot-stats-enabled", None, MODIFIABLE | MULTI_ARG, CLUSTER_SLOT_STATS, 0).apply(memory_tracking),
    boolean("lua-enable-deprecated-api", None, IMMUTABLE | HIDDEN, false),
    boolean("key-memory-histograms", None, MODIFIABLE, false).apply(memory_tracking),
    // String configs
    string("aclfile", None, IMMUTABLE, false, Some("")),
    string("unixsocket", None, IMMUTABLE, true, None),
    string("pidfile", None, IMMUTABLE, true, None),
    string("replica-announce-ip", Some("slave-announce-ip"), MODIFIABLE, true, None),
    Param::new("masteruser", None, MODIFIABLE | SENSITIVE, Ty::Str { default: None, null_empty: true })
        .at(|c| Val::Str(Some(c.masteruser.clone()).filter(|s| !s.is_empty())), |c, v| c.masteruser = v.string().unwrap_or_default()),
    string("cluster-announce-ip", None, MODIFIABLE, true, None).check(|_, v| str_check(v, valid_cluster_announce_ip)),
    string("cluster-config-file", None, IMMUTABLE, false, Some("nodes.conf")),
    string("cluster-announce-hostname", None, MODIFIABLE, true, None).check(|_, v| str_check(v, valid_announced_hostname)),
    string("cluster-announce-human-nodename", None, MODIFIABLE, true, None).check(|_, v| {
        str_check(v, |s| {
            if s.bytes().all(|c| !(c.is_ascii_control()) && (c.is_ascii_alphanumeric() || !b"!#$%&()*+:;<>?@[]^{|}~,= \"'\\".contains(&c))) {
                Ok(())
            } else {
                Err("Announced human node name contained invalid character")
            }
        })
    }),
    string("syslog-ident", None, IMMUTABLE, false, Some("redis")),
    string("dbfilename", None, MODIFIABLE | PROTECTED, false, Some("dump.rdb"))
        .check(|_, v| str_check(v, |s| if is_base_name(s) { Ok(()) } else { Err("dbfilename can't be a path, just a filename") })),
    string("appendfilename", None, IMMUTABLE, false, Some("appendonly.aof")),
    string("appenddirname", None, IMMUTABLE, false, Some("appendonlydir")),
    string("backupdirname", None, IMMUTABLE, true, Some("backupdir")),
    string("preload-file", None, IMMUTABLE, true, None),
    string("server-cpulist", Some("server_cpulist"), IMMUTABLE, true, None),
    string("bio-cpulist", Some("bio_cpulist"), IMMUTABLE, true, None),
    string("aof-rewrite-cpulist", Some("aof_rewrite_cpulist"), IMMUTABLE, true, None),
    string("bgsave-cpulist", Some("bgsave_cpulist"), IMMUTABLE, true, None),
    string("ignore-warnings", None, MODIFIABLE, false, Some("")),
    string("proc-title-template", None, MODIFIABLE, false, Some("{title} {listen-addr} {server-mode}")).check(|_, v| {
        str_check(v, |s| if valid_proc_title_template(s) { Ok(()) } else { Err("template format is invalid or contains unknown variables") })
    }),
    string("bind-source-addr", None, MODIFIABLE, true, None),
    string("logfile", None, IMMUTABLE, false, Some("")),
    string("locale-collate", None, MODIFIABLE, false, Some("")),
    // SDS configs
    Param::new("masterauth", None, MODIFIABLE | SENSITIVE, Ty::Str { default: None, null_empty: true })
        .at(|c| Val::Str(Some(c.masterauth.clone()).filter(|s| !s.is_empty())), |c, v| c.masterauth = v.string().unwrap_or_default()),
    Param::new("requirepass", None, MODIFIABLE | SENSITIVE, Ty::Str { default: None, null_empty: true })
        .at(|c| Val::Str(c.requirepass.clone()), |c, v| c.requirepass = v.string()),
    // Enum configs
    enumeration("supervised", None, IMMUTABLE, SUPERVISED, 0),
    enumeration("syslog-facility", None, IMMUTABLE, SYSLOG_FACILITY, 128),
    enumeration("repl-diskless-load", None, DEBUG | MODIFIABLE | DENY_LOADING, REPL_DISKLESS_LOAD, 0),
    enumeration("loglevel", None, MODIFIABLE, LOGLEVEL, 2),
    Param::new("maxmemory-policy", None, MODIFIABLE, Ty::Enum { values: POLICIES, default: 9 })
        .at(|c| Val::Num(c.maxmemory_policy.index() as i128), |c, v| c.maxmemory_policy = Policy::ALL[v.num() as usize]),
    enumeration("appendfsync", None, MODIFIABLE, AOF_FSYNC, 2),
    enumeration("oom-score-adj", None, MODIFIABLE, OOM_SCORE_ADJ, 0),
    enumeration("acl-pubsub-default", None, MODIFIABLE, ACL_PUBSUB_DEFAULT, 0),
    enumeration("sanitize-dump-payload", None, DEBUG | MODIFIABLE, SANITIZE_DUMP, 0),
    at!(enumeration("enable-protected-configs", None, IMMUTABLE, PROTECTED_ACTION, 0), n!(enable_protected_configs)),
    at!(enumeration("enable-debug-command", None, IMMUTABLE, PROTECTED_ACTION, 0), n!(enable_debug_command)),
    at!(enumeration("enable-module-command", None, IMMUTABLE, PROTECTED_ACTION, 0), n!(enable_module_command)),
    enumeration("cluster-preferred-endpoint-type", None, MODIFIABLE, ENDPOINT_TYPE, 0),
    enumeration("propagation-error-behavior", None, MODIFIABLE, PROPAGATION_ERROR, 0),
    enumeration("shutdown-on-sigint", None, MODIFIABLE | MULTI_ARG, SHUTDOWN_ON_SIG, 0).check(shutdown_flags),
    enumeration("shutdown-on-sigterm", None, MODIFIABLE | MULTI_ARG, SHUTDOWN_ON_SIG, 0).check(shutdown_flags),
    // Integer configs
    at!(signed("databases", None, IMMUTABLE, 1, INT_MAX, 16, INTEGER), n!(databases)),
    signed("port", None, MODIFIABLE, 0, 65535, PORT, INTEGER).apply(|c| {
        if port(c) != PORT { Err("Unable to listen on this port. Check server logs.".into()) } else { Ok(()) }
    }),
    signed("io-threads", None, DEBUG | IMMUTABLE, 1, 128, 1, INTEGER),
    signed("prefetch-batch-max-size", None, MODIFIABLE | HIDDEN, 0, 128, 16, INTEGER),
    signed("auto-aof-rewrite-percentage", None, MODIFIABLE, 0, INT_MAX, 100, INTEGER),
    signed("cluster-replica-validity-factor", Some("cluster-slave-validity-factor"), MODIFIABLE, 0, INT_MAX, 10, INTEGER),
    at!(signed("list-max-listpack-size", Some("list-max-ziplist-size"), MODIFIABLE, INT_MIN, INT_MAX, -2, INTEGER), n!(list_max_listpack_size)),
    signed("tcp-keepalive", None, MODIFIABLE, 0, INT_MAX, 300, INTEGER),
    signed("cluster-migration-barrier", None, MODIFIABLE, 0, INT_MAX, 1, INTEGER),
    signed("active-defrag-cycle-min", None, MODIFIABLE, 1, 99, 1, INTEGER),
    signed("active-defrag-cycle-max", None, MODIFIABLE, 1, 99, 25, INTEGER),
    signed("active-defrag-threshold-lower", None, MODIFIABLE, 0, 1000, 10, INTEGER),
    signed("active-defrag-threshold-upper", None, MODIFIABLE, 0, 1000, 100, INTEGER),
    at!(signed("lfu-log-factor", None, MODIFIABLE, 0, INT_MAX, 10, INTEGER), n!(lfu_log_factor)),
    at!(signed("lfu-decay-time", None, MODIFIABLE, 0, INT_MAX, 1, INTEGER), n!(lfu_decay_time)),
    signed("replica-priority", Some("slave-priority"), MODIFIABLE, 0, INT_MAX, 100, INTEGER),
    signed("repl-diskless-sync-delay", None, MODIFIABLE, 0, INT_MAX, 5, INTEGER),
    at!(signed("maxmemory-samples", None, MODIFIABLE, 1, 64, 5, INTEGER), n!(maxmemory_samples)),
    at!(signed("maxmemory-eviction-tenacity", None, MODIFIABLE, 0, 100, 10, INTEGER), n!(maxmemory_eviction_tenacity)),
    at!(signed("timeout", None, MODIFIABLE, 0, INT_MAX, 0, INTEGER), n!(timeout)),
    signed("replica-announce-port", Some("slave-announce-port"), MODIFIABLE, 0, 65535, 0, INTEGER),
    signed("tcp-backlog", None, IMMUTABLE, 0, INT_MAX, 511, INTEGER),
    signed("cluster-port", None, IMMUTABLE, 0, 65535, 0, INTEGER),
    signed("cluster-announce-bus-port", None, MODIFIABLE, 0, 65535, 0, INTEGER),
    signed("cluster-announce-port", None, MODIFIABLE, 0, 65535, 0, INTEGER),
    signed("cluster-announce-tls-port", None, MODIFIABLE, 0, 65535, 0, INTEGER),
    signed("repl-timeout", None, MODIFIABLE, 1, INT_MAX, 60, INTEGER),
    signed("repl-ping-replica-period", Some("repl-ping-slave-period"), MODIFIABLE, 1, INT_MAX, 10, INTEGER),
    at!(signed("list-compress-depth", None, DEBUG | MODIFIABLE, 0, INT_MAX, 0, INTEGER), n!(list_compress_depth)),
    at!(signed("rdb-key-save-delay", None, MODIFIABLE | HIDDEN, INT_MIN, INT_MAX, 0, INTEGER), n!(rdb_key_save_delay)),
    at!(signed("key-load-delay", None, MODIFIABLE | HIDDEN, INT_MIN, INT_MAX, 0, INTEGER), n!(key_load_delay)),
    at!(signed("active-expire-effort", None, MODIFIABLE, 1, 10, 1, INTEGER), n!(active_expire_effort)),
    // Out-of-range values are a hint: capped to 1..=500.
    at!(signed("hz", None, MODIFIABLE, 0, INT_MAX, 10, INTEGER), n!(hz)).apply(|c| {
        c.hz = c.hz.clamp(1, 500);
        Ok(())
    }),
    signed("min-replicas-to-write", Some("min-slaves-to-write"), MODIFIABLE, 0, INT_MAX, 0, INTEGER),
    signed("min-replicas-max-lag", Some("min-slaves-max-lag"), MODIFIABLE, 0, INT_MAX, 10, INTEGER),
    signed("watchdog-period", None, MODIFIABLE | HIDDEN, 0, INT_MAX, 0, INTEGER),
    signed("shutdown-timeout", None, MODIFIABLE, 0, INT_MAX, 10, INTEGER),
    signed("repl-diskless-sync-max-replicas", None, MODIFIABLE, 0, INT_MAX, 0, INTEGER),
    signed("cluster-compatibility-sample-ratio", None, MODIFIABLE, 0, 100, 0, INTEGER),
    signed("cluster-slot-migration-max-archived-tasks", None, MODIFIABLE | HIDDEN, 1, INT_MAX, 32, INTEGER),
    signed("lookahead", None, MODIFIABLE, 1, INT_MAX, 16, INTEGER),
    at!(signed("slowlog-entry-max-argc", None, MODIFIABLE, 2, INT_MAX, 32, INTEGER), n!(slowlog_max_argc)),
    // Unsigned int configs
    at!(unsigned("maxclients", None, MODIFIABLE, 1, UINT_MAX, 10000, INTEGER), n!(maxclients)),
    unsigned("unixsocketperm", None, IMMUTABLE, 0, 0o777, 0, OCTAL),
    unsigned("socket-mark-id", None, IMMUTABLE, 0, UINT_MAX, 0, INTEGER),
    unsigned("max-new-connections-per-cycle", None, MODIFIABLE, 1, 1000, 10, INTEGER),
    unsigned("max-new-tls-connections-per-cycle", None, MODIFIABLE, 1, 1000, 1, INTEGER),
    at!(unsigned("array-slice-size", None, MODIFIABLE, 256, 65536, 4096, INTEGER), n!(array_slice_size)).check(|_, v| {
        let n = v.num();
        if n <= 0 || n & (n - 1) != 0 { Err("array-slice-size must be a power of two".into()) } else { Ok(()) }
    }),
    at!(unsigned("array-sparse-kmax", None, MODIFIABLE, 0, 256, 10, INTEGER), n!(array_sparse_kmax)).check(|c, v| {
        if v.num() > 0 && v.num() <= c.array_sparse_kmin as i128 {
            Err("array-sparse-kmax must be greater than array-sparse-kmin when non-zero".into())
        } else {
            Ok(())
        }
    }),
    at!(unsigned("array-sparse-kmin", None, MODIFIABLE, 0, 256, 5, INTEGER), n!(array_sparse_kmin)).check(|c, v| {
        if c.array_sparse_kmax > 0 && v.num() >= c.array_sparse_kmax as i128 {
            Err("array-sparse-kmin must be less than array-sparse-kmax".into())
        } else {
            Ok(())
        }
    }),
    // Unsigned long configs
    signed("active-defrag-max-scan-fields", None, MODIFIABLE, 1, LONG_MAX, 1000, INTEGER),
    at!(signed("slowlog-max-len", None, MODIFIABLE, 0, LONG_MAX, 128, INTEGER), n!(slowlog_max_len)),
    at!(signed("slowlog-entry-max-string-len", None, MODIFIABLE, 1, LONG_MAX, 128, INTEGER), n!(slowlog_max_string_len)),
    at!(signed("acllog-max-len", None, MODIFIABLE, 0, LONG_MAX, 128, INTEGER), n!(acllog_max_len)),
    // Long long configs
    at!(signed("busy-reply-threshold", Some("lua-time-limit"), MODIFIABLE, 0, LONG_MAX, 5000, INTEGER), n!(busy_reply_threshold)),
    signed("cluster-node-timeout", None, MODIFIABLE, 0, LONG_MAX, 15000, INTEGER),
    signed("cluster-ping-interval", None, MODIFIABLE | HIDDEN, 0, LONG_MAX, 0, INTEGER),
    signed("cluster-slot-migration-handoff-max-lag-bytes", None, MODIFIABLE, 0, LONG_MAX, 1 << 20, MEMORY),
    signed("cluster-slot-migration-write-pause-timeout", None, MODIFIABLE, 0, LONG_MAX, 10000, INTEGER),
    signed("cluster-slot-migration-sync-buffer-drain-timeout", None, MODIFIABLE | HIDDEN, 0, LONG_MAX, 60000, INTEGER),
    at!(signed("slowlog-log-slower-than", None, MODIFIABLE, -1, LONG_MAX, 10000, INTEGER), n!(slowlog_log_slower_than)),
    at!(signed("latency-monitor-threshold", None, MODIFIABLE, 0, LONG_MAX, 0, INTEGER), n!(latency_monitor_threshold)),
    at!(signed("proto-max-bulk-len", None, DEBUG | MODIFIABLE, 1024 * 1024, LONG_MAX, 512 * 1024 * 1024, MEMORY), n!(proto_max_bulk_len)),
    at!(signed("stream-node-max-entries", None, MODIFIABLE, 0, LONG_MAX, 100, INTEGER), n!(stream_node_max_entries)),
    at!(signed("stream-idmp-duration", None, MODIFIABLE, 1, 86400, 100, INTEGER), n!(stream_idmp_duration)),
    at!(signed("stream-idmp-maxsize", None, MODIFIABLE, 1, 10000, 100, INTEGER), n!(stream_idmp_maxsize)),
    signed("repl-backlog-size", None, MODIFIABLE, 1, LONG_MAX, 1 << 20, MEMORY),
    signed("replica-full-sync-buffer-limit", None, MODIFIABLE, 0, LONG_MAX, 0, MEMORY),
    // Unsigned long long configs
    Param::new("maxmemory", None, MODIFIABLE, Ty::Num { min: 0, max: ULLONG_MAX, default: 0, unsigned: true, nf: MEMORY })
        .at(|c| Val::Num(c.maxmemory as i128), |c, v| c.maxmemory = v.num() as u64)
        .check(|c, v| {
            // Hosted databases can lower their limit but not exceed their plan.
            let m = v.num() as u64;
            if c.maxmemory_cap > 0 && (m == 0 || m > c.maxmemory_cap) {
                return Err(format!("maxmemory can't exceed this database's plan limit of {} bytes", c.maxmemory_cap));
            }
            Ok(())
        }),
    unsigned("cluster-link-sendbuf-limit", None, MODIFIABLE, 0, ULLONG_MAX, 0, MEMORY),
    // Size_t configs
    at!(unsigned("hash-max-listpack-entries", Some("hash-max-ziplist-entries"), MODIFIABLE, 0, LONG_MAX, 512, INTEGER), n!(hash_max_listpack_entries)),
    at!(unsigned("hash-min-template-entries", None, MODIFIABLE, 0, LONG_MAX, 0, INTEGER), n!(hash_min_template_entries)),
    at!(unsigned("hash-max-template-entries", None, MODIFIABLE, 0, LONG_MAX, 0, INTEGER), n!(hash_max_template_entries)),
    at!(unsigned("hash-rdb-load-min-template-entries", None, MODIFIABLE, 0, LONG_MAX, 0, INTEGER), n!(hash_rdb_load_min_template_entries)),
    at!(unsigned("hash-rdb-load-max-template-entries", None, MODIFIABLE, 0, LONG_MAX, 0, INTEGER), n!(hash_rdb_load_max_template_entries)),
    at!(
        unsigned("hash-rdb-load-template-disassembly-threshold", None, MODIFIABLE, 0, LONG_MAX, 0, INTEGER),
        n!(hash_rdb_load_template_disassembly_threshold)
    ),
    at!(unsigned("set-max-intset-entries", None, MODIFIABLE, 0, LONG_MAX, 512, INTEGER), n!(set_max_intset_entries)),
    at!(unsigned("set-max-listpack-entries", None, MODIFIABLE, 0, LONG_MAX, 128, INTEGER), n!(set_max_listpack_entries)),
    at!(unsigned("set-max-listpack-value", None, MODIFIABLE, 0, LONG_MAX, 64, INTEGER), n!(set_max_listpack_value)),
    at!(unsigned("zset-max-listpack-entries", Some("zset-max-ziplist-entries"), MODIFIABLE, 0, LONG_MAX, 128, INTEGER), n!(zset_max_listpack_entries)),
    unsigned("active-defrag-ignore-bytes", None, MODIFIABLE, 1, LONG_MAX, 100 << 20, MEMORY),
    at!(unsigned("hash-max-listpack-value", Some("hash-max-ziplist-value"), MODIFIABLE, 0, LONG_MAX, 64, MEMORY), n!(hash_max_listpack_value)),
    at!(unsigned("stream-node-max-bytes", None, MODIFIABLE, 0, LONG_MAX, 4096, MEMORY), n!(stream_node_max_bytes)),
    at!(unsigned("zset-max-listpack-value", Some("zset-max-ziplist-value"), MODIFIABLE, 0, LONG_MAX, 64, MEMORY), n!(zset_max_listpack_value)),
    at!(unsigned("hll-sparse-max-bytes", None, MODIFIABLE, 0, LONG_MAX, 3000, MEMORY), n!(hll_sparse_max_bytes)),
    at!(unsigned("tracking-table-max-keys", None, MODIFIABLE, 0, LONG_MAX, 1000000, INTEGER), n!(tracking_table_max_keys)),
    at!(
        unsigned("client-query-buffer-limit", None, DEBUG | MODIFIABLE, 1024 * 1024, LONG_MAX, 1024 * 1024 * 1024, MEMORY),
        n!(client_query_buffer_limit)
    ),
    signed("maxmemory-clients", None, MODIFIABLE, -100, LONG_MAX, 0, MEMORY | PERCENT),
    // Other configs
    signed("repl-backlog-ttl", None, MODIFIABLE, 0, LONG_MAX, 3600, INTEGER),
    signed("backup-sealed-ttl", None, MODIFIABLE, 0, LONG_MAX, 0, INTEGER),
    signed("auto-aof-rewrite-min-size", None, MODIFIABLE, 0, LONG_MAX, 64 << 20, MEMORY),
    signed("loading-process-events-interval-bytes", None, MODIFIABLE | HIDDEN, 1024, INT_MAX, 1024 * 512, INTEGER),
    signed("aof-load-corrupt-tail-max-size", None, MODIFIABLE, 0, LONG_MAX, 0, INTEGER),
    signed("tls-port", None, MODIFIABLE, 0, 65535, 0, INTEGER).apply(|_| Err("Unable to update TLS configuration. Check server logs.".into())),
    signed("tls-session-cache-size", None, MODIFIABLE, 0, INT_MAX, 20 * 1024, INTEGER).apply(apply_tls),
    signed("tls-session-cache-timeout", None, MODIFIABLE, 0, INT_MAX, 300, INTEGER).apply(apply_tls),
    boolean("tls-cluster", None, MODIFIABLE, false).apply(apply_tls),
    boolean("tls-replication", None, MODIFIABLE, false).apply(apply_tls),
    enumeration("tls-auth-clients", None, MODIFIABLE, TLS_AUTH_CLIENTS, 1),
    enumeration("tls-auth-clients-user", None, MODIFIABLE, TLS_CLIENT_AUTH_USER, 0),
    boolean("tls-prefer-server-ciphers", None, MODIFIABLE, false).apply(apply_tls),
    boolean("tls-session-caching", None, MODIFIABLE, true).apply(apply_tls),
    string("tls-cert-file", None, VOLATILE | MODIFIABLE, true, None).apply(apply_tls),
    string("tls-key-file", None, VOLATILE | MODIFIABLE, true, None).apply(apply_tls),
    string("tls-key-file-pass", None, MODIFIABLE | SENSITIVE, true, None).apply(apply_tls),
    string("tls-client-cert-file", None, VOLATILE | MODIFIABLE, true, None).apply(apply_tls),
    string("tls-client-key-file", None, VOLATILE | MODIFIABLE, true, None).apply(apply_tls),
    string("tls-client-key-file-pass", None, MODIFIABLE | SENSITIVE, true, None).apply(apply_tls),
    string("tls-dh-params-file", None, VOLATILE | MODIFIABLE, true, None).apply(apply_tls),
    string("tls-ca-cert-file", None, VOLATILE | MODIFIABLE, true, None).apply(apply_tls),
    string("tls-ca-cert-dir", None, VOLATILE | MODIFIABLE, true, None).apply(apply_tls),
    string("tls-protocols", None, MODIFIABLE, true, None).apply(apply_tls),
    string("tls-ciphers", None, MODIFIABLE, true, None).apply(apply_tls),
    string("tls-ciphersuites", None, MODIFIABLE, true, None).apply(apply_tls),
    string("tls-expected-peer-name", None, MODIFIABLE, true, None).check(|_, v| str_check(v, valid_expected_peer_name)),
    // Special configs
    special("dir", None, MODIFIABLE | PROTECTED | DENY_LOADING, |_| "/data".into(), |_, _| Ok(true)),
    special("save", None, MODIFIABLE | MULTI_ARG, get_save, set_save),
    special("client-output-buffer-limit", None, MODIFIABLE | MULTI_ARG, get_obuf, set_obuf),
    special("oom-score-adj-values", None, MODIFIABLE | MULTI_ARG, get_oom_values, set_oom_values),
    special("notify-keyspace-events", None, MODIFIABLE, |c| notify::flags_to_string(c.notify), |c, argv| {
        if argv.len() != 1 {
            return Err("wrong number of arguments".into());
        }
        c.notify = notify::parse_flags(argv[0]).ok_or("Invalid event class character. Use 'Ag$lshzxeKEtmdnocaSTIV'.")?;
        Ok(true)
    }),
    special("bind", None, MODIFIABLE | MULTI_ARG, get_bind, |c, argv| {
        if argv.len() > 16 {
            return Err("Too many bind addresses specified.".into());
        }
        let argv = if argv.len() == 1 && argv[0].is_empty() { &[][..] } else { argv };
        c.extra.insert("bind", Val::Str(Some(argv.join(" "))));
        Ok(true)
    })
    .apply(|c| if get_bind(c) != BIND { Err("Failed to bind to specified addresses.".into()) } else { Ok(()) }),
    special("replicaof", Some("slaveof"), IMMUTABLE | MULTI_ARG, |_| String::new(), |_, _| Ok(true)),
    special("latency-tracking-info-percentiles", None, MODIFIABLE | MULTI_ARG, get_percentiles, set_percentiles),
];

fn port(c: &Config) -> i128 {
    c.extra.get("port").map_or(PORT, Val::num)
}

fn get_bind(c: &Config) -> String {
    c.extra.get("bind").cloned().and_then(Val::string).unwrap_or_else(|| BIND.into())
}

/// `applyTlsCfg`: TLS can't be configured here, so enabling any TLS listener fails.
fn apply_tls(c: &mut Config) -> Result<(), String> {
    let on = |n: &str| c.extra.get(n).is_some_and(|v| v.num() != 0);
    if on("tls-port") || on("tls-replication") || on("tls-cluster") {
        return Err("Unable to update TLS configuration. Check server logs.".into());
    }
    Ok(())
}

/// `updateMemoryTrackingEnabled`: per-key memory tracking is decided at startup.
fn memory_tracking(c: &mut Config) -> Result<(), String> {
    let hist = c.extra.get("key-memory-histograms").is_some_and(|v| v.num() != 0);
    let slot_mem = c.extra.get("cluster-slot-stats-enabled").is_some_and(|v| v.num() & 4 != 0);
    if hist || slot_mem {
        return Err("memory tracking cannot be enabled at runtime".into());
    }
    Ok(())
}

fn shutdown_flags(_: &Config, v: &Val) -> Result<(), String> {
    if v.num() & 1 != 0 && v.num() & 2 != 0 {
        return Err("shutdown options SAVE and NOSAVE can't be used simultaneously".into());
    }
    Ok(())
}

fn str_check(v: &Val, f: impl Fn(&str) -> Result<(), &'static str>) -> Result<(), String> {
    match v {
        Val::Str(Some(s)) => f(s).map_err(String::from),
        _ => f("").map_err(String::from),
    }
}

fn is_base_name(s: &str) -> bool {
    !s.contains('/') && !s.contains('\\')
}

fn valid_hostname_chars(s: &str) -> Result<(), &'static str> {
    if s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.') {
        Ok(())
    } else {
        Err("Hostnames may only contain alphanumeric characters, hyphens or dots")
    }
}

fn valid_announced_hostname(s: &str) -> Result<(), &'static str> {
    if s.len() >= 256 {
        return Err("Hostnames must be less than 256 characters");
    }
    valid_hostname_chars(s)
}

fn valid_cluster_announce_ip(s: &str) -> Result<(), &'static str> {
    if s.is_empty() || s.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    if s.len() >= 46 {
        return Err("Hostnames for cluster-announce-ip must be less than 46 characters");
    }
    valid_hostname_chars(s)
}

fn valid_expected_peer_name(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Ok(());
    }
    let mut has_name = false;
    for c in s.bytes() {
        if c == b' ' {
            continue;
        }
        if matches!(c, b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
            return Err("tls-expected-peer-name must not contain whitespace other than spaces separating names; use an empty string to disable it");
        }
        has_name = true;
    }
    if !has_name {
        return Err("tls-expected-peer-name contains no usable name; use an empty string to disable it");
    }
    Ok(())
}

/// `validateProcTitleTemplate`: every `{variable}` known, and a non-empty expansion.
fn valid_proc_title_template(t: &str) -> bool {
    let mut out = String::new();
    let mut p = t;
    while !p.is_empty() {
        let Some(sv) = p.find('{') else {
            out.push_str(p);
            break;
        };
        out.push_str(&p[..sv]);
        let rest = &p[sv + 1..];
        if rest.is_empty() {
            return false;
        }
        if let Some(r) = rest.strip_prefix('{') {
            out.push('{');
            p = r;
            continue;
        }
        let Some(ev) = rest.find('}') else { return false };
        out.push_str(match &rest[..ev] {
            "title" | "server-mode" | "unixsocket" => "",
            "listen-addr" => "*:6379",
            "config-file" => "-",
            "port" => "6379",
            "tls-port" => "0",
            _ => return false,
        });
        p = &rest[ev + 1..];
    }
    !out.is_empty()
}

fn get_save(c: &Config) -> String {
    c.save.iter().map(|(s, n)| format!("{s} {n}")).collect::<Vec<_>>().join(" ")
}

fn set_save(c: &mut Config, argv: &[&str]) -> Result<bool, String> {
    let argv = if argv.len() == 1 && argv[0].is_empty() { &[][..] } else { argv };
    if !argv.len().is_multiple_of(2) {
        return Err("Invalid save parameters".into());
    }
    let mut rules = Vec::new();
    for (j, a) in argv.iter().enumerate() {
        let v = crate::util::strtoll_full(a.as_bytes()).ok_or("Invalid save parameters")?;
        if (j % 2 == 0 && v < 1) || (j % 2 == 1 && v < 0) {
            return Err("Invalid save parameters".into());
        }
        if j % 2 == 1 {
            rules.push((crate::util::strtoll_full(argv[j - 1].as_bytes()).unwrap_or(0), v as i32));
        }
    }
    c.save = rules;
    Ok(true)
}

fn get_obuf(c: &Config) -> String {
    ["normal", "slave", "pubsub"]
        .iter()
        .zip(&c.obuf_limits)
        .map(|(n, l)| format!("{n} {} {} {}", l.hard, l.soft, l.soft_seconds))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `updateClientOutputBufferLimit`.
fn set_obuf(c: &mut Config, argv: &[&str]) -> Result<bool, String> {
    if !argv.len().is_multiple_of(4) {
        return Err("Wrong number of arguments in buffer limit configuration.".into());
    }
    let mut next = c.obuf_limits;
    for a in argv.chunks(4) {
        let class = match a[0].to_ascii_lowercase().as_str() {
            "normal" => 0,
            "slave" | "replica" => 1,
            "pubsub" => 2,
            _ => return Err("Invalid client class specified in buffer limit configuration.".into()),
        };
        let bad = || "Error in hard, soft or soft_seconds setting in buffer limit configuration.".to_string();
        let hard = memtoull(a[1]).ok_or_else(bad)?;
        let soft = memtoull(a[2]).ok_or_else(bad)?;
        let secs = crate::util::strtoll_full(a[3].as_bytes()).ok_or_else(bad)? as i32;
        if secs < 0 {
            return Err(bad());
        }
        next[class] = ObufLimit { hard, soft, soft_seconds: secs as i64 };
    }
    c.obuf_limits = next;
    Ok(true)
}

fn get_oom_values(c: &Config) -> String {
    c.extra.get("oom-score-adj-values").cloned().and_then(Val::string).unwrap_or_else(|| "0 200 800".into())
}

fn set_oom_values(c: &mut Config, argv: &[&str]) -> Result<bool, String> {
    if argv.len() != 3 {
        return Err("wrong number of arguments".into());
    }
    let mut vals = Vec::new();
    for a in argv {
        let v = crate::util::strtoll_full(a.as_bytes())
            .filter(|v| (-2000..=2000).contains(v) && !a.is_empty())
            .ok_or("Invalid oom-score-adj-values, elements must be between -2000 and 2000.")?;
        vals.push(v.to_string());
    }
    let s = vals.join(" ");
    let changed = s != get_oom_values(c);
    c.extra.insert("oom-score-adj-values", Val::Str(Some(s)));
    Ok(changed)
}

fn get_percentiles(c: &Config) -> String {
    c.latency_percentiles.iter().map(|p| crate::commands::info::trim_double(*p)).collect::<Vec<_>>().join(" ")
}

fn set_percentiles(c: &mut Config, argv: &[&str]) -> Result<bool, String> {
    let argv = if argv.len() == 1 && argv[0].is_empty() { &[][..] } else { argv };
    let mut out = Vec::new();
    for a in argv {
        let p = redis_proto::parse_double(a.as_bytes()).ok_or("Invalid latency-tracking-info-percentiles parameters")?;
        if !(0.0..=100.0).contains(&p) {
            return Err("latency-tracking-info-percentiles parameters should sit between [0.0,100.0]".into());
        }
        out.push(p);
    }
    c.latency_percentiles = out;
    Ok(true)
}

/// The parameter named `name` (or one of its aliases), case-insensitively.
pub fn find(name: &str) -> Option<&'static Param> {
    PARAMS.iter().find(|p| p.name.eq_ignore_ascii_case(name) || p.alias.is_some_and(|a| a.eq_ignore_ascii_case(name)))
}

/// Is `name` a parameter whose value is redacted from logs (`SENSITIVE_CONFIG`)?
pub fn is_sensitive(name: &[u8]) -> bool {
    std::str::from_utf8(name).ok().and_then(find).is_some_and(|p| p.flags & SENSITIVE != 0)
}

/// Why CONFIG SET refused.
#[derive(Debug, PartialEq, Eq)]
pub enum SetError {
    /// `Unknown option or number of arguments for CONFIG SET - '<name>'`.
    Unknown(String),
    /// `CONFIG SET failed (possibly related to argument '<name>') - <reason>`.
    Failed(String, String),
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetError::Unknown(n) => write!(f, "Unknown option or number of arguments for CONFIG SET - '{n}'"),
            SetError::Failed(n, e) => write!(f, "CONFIG SET failed (possibly related to argument '{n}') - {e}"),
        }
    }
}

impl Config {
    /// `CONFIG GET pattern`: matching `(name, value)` pairs, aliases included like Redis. A name
    /// without glob characters is looked up directly (hidden parameters too) and echoed as given.
    pub fn get(&self, pattern: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if !pattern.iter().any(|c| b"[*?".contains(c)) {
            let name = String::from_utf8_lossy(pattern);
            if let Some(p) = find(&name) {
                out.push((name.into_owned(), p.get(self)));
            }
            return out;
        }
        for p in PARAMS.iter().filter(|p| p.flags & HIDDEN == 0) {
            for name in std::iter::once(p.name).chain(p.alias) {
                if crate::util::glob_match(pattern, name.as_bytes(), true) {
                    out.push((name.to_string(), p.get(self)));
                }
            }
        }
        out
    }

    /// One parameter, like a one-pair CONFIG SET.
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        self.set_many(&[(name, value)], true).map(|_| ()).map_err(|e| e.to_string())
    }

    /// `configSetCommand`: all pairs or none. `protected_ok` says whether this client may change
    /// `PROTECTED_CONFIG` parameters. Returns the primary names of the parameters set.
    pub fn set_many(&mut self, pairs: &[(&str, &str)], protected_ok: bool) -> Result<Vec<&'static str>, SetError> {
        let mut params: Vec<&'static Param> = Vec::new();
        let mut first_err: Option<SetError> = None;
        for (name, _) in pairs {
            // The first problem found is the one reported.
            if first_err.is_some() {
                continue;
            }
            let Some(p) = find(name) else {
                first_err = Some(SetError::Unknown(name.to_string()));
                continue;
            };
            if p.flags & IMMUTABLE != 0 || (p.flags & PROTECTED != 0 && !protected_ok) {
                let why = if p.flags & IMMUTABLE != 0 { "can't set immutable config" } else { "can't set protected config" };
                first_err = Some(SetError::Failed(name.to_string(), why.into()));
                continue;
            }
            if params.iter().any(|q| std::ptr::eq(*q, p)) {
                first_err = Some(SetError::Failed(name.to_string(), "duplicate parameter".into()));
            }
            params.push(p);
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        let backup = self.clone();
        let mut changed = Vec::new();
        for ((name, value), p) in pairs.iter().zip(&params) {
            match p.set(self, value) {
                Ok(c) => changed.push(c),
                Err(e) => {
                    *self = backup;
                    return Err(SetError::Failed(name.to_ascii_lowercase(), e));
                }
            }
        }
        for ((name, _), (p, c)) in pairs.iter().zip(params.iter().zip(changed)) {
            if let Some(apply) = p.apply.filter(|_| c)
                && let Err(e) = apply(self)
            {
                *self = backup;
                return Err(SetError::Failed(name.to_ascii_lowercase(), e));
            }
        }
        Ok(params.iter().map(|p| p.name).collect())
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
        assert!(c.set("daemonize", "yes").is_err());
        assert!(c.set("nope", "1").is_err());
        assert!(c.set("maxmemory-samples", "0").is_err());
        assert!(c.get(b"*max-*-entries*").len() >= 4);
        c.set("hz", "1000").unwrap();
        assert_eq!(c.hz, 500);
        c.set("maxmemory-clients", "10%").unwrap();
        assert_eq!(c.get(b"maxmemory-clients")[0].1, "10%");
        assert_eq!(c.set("maxmemory-clients", "101%").unwrap_err(), "CONFIG SET failed (possibly related to argument 'maxmemory-clients') - percentage argument must be less or equal to 100");
        c.set("shutdown-on-sigint", "nosave now").unwrap();
        assert_eq!(c.get(b"shutdown-on-sigint")[0].1, "nosave now");
        assert!(c.set("shutdown-on-sigint", "save nosave").is_err());
        c.set("save", "10 2 20 4").unwrap();
        assert_eq!(c.get(b"save")[0].1, "10 2 20 4");
        c.set("client-output-buffer-limit", "replica 1mb 2mb 3").unwrap();
        assert_eq!(c.get(b"client-output-buffer-limit")[0].1, "normal 0 0 0 slave 1048576 2097152 3 pubsub 33554432 8388608 60");
        assert!(c.set("tls-replication", "yes").is_err());
        assert_eq!(c.get(b"tls-replication")[0].1, "no");
    }

    #[test]
    fn set_many_is_atomic() {
        let mut c = Config::default();
        let e = c.set_many(&[("maxmemory", "1mb"), ("timeout", "x")], true).unwrap_err();
        assert_eq!(e, SetError::Failed("timeout".into(), "argument couldn't be parsed into an integer".into()));
        assert_eq!(c.maxmemory, 0);
        assert_eq!(c.set_many(&[("x", "1"), ("daemonize", "yes"), ("y", "1")], true).unwrap_err(), SetError::Unknown("x".into()));
        assert!(matches!(c.set_many(&[("timeout", "1"), ("TIMEOUT", "2")], true).unwrap_err(), SetError::Failed(n, e) if n == "TIMEOUT" && e == "duplicate parameter"));
        assert!(c.set_many(&[("dir", "/x")], false).is_err());
    }

    #[test]
    fn memory_values() {
        assert_eq!(memtoull("1KB"), Some(1024));
        assert_eq!(memtoull("5k"), Some(5000));
        assert_eq!(memtoull(""), None);
        assert_eq!(memtoull("-1"), None);
        assert_eq!(memtoull("99999999999999999999gb"), Some(u64::MAX));
    }
}
