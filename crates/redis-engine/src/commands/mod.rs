//! The command table and helpers shared by command implementations.
//!
//! Each family lives in its own module and exports `COMMANDS`. A [`Command`] carries what Redis'
//! command table does: arity, flags, key positions (for cluster routing, ACL key checks and
//! `COMMAND GETKEYS`), its group (ACL categories, `COMMAND DOCS`) and optional subcommands.

pub mod acl;
pub mod arrays;
pub mod bitmaps;
pub mod bloom;
pub mod debug;
pub mod geo;
pub mod hashes;
pub mod hll;
pub mod keys;
pub mod meta;
pub mod client;
pub mod hotkeys;
pub mod info;
pub mod json;
pub mod lolwut;
pub mod persist;
pub mod replication;
pub mod keyspec;
pub mod migrate;
pub mod lists;
pub mod multi;
pub mod pubsub;
pub mod scripting;
pub mod server;
pub mod sets;
pub mod sketch;
pub mod sort;
pub mod streams;
pub mod strings;
pub mod zsets;

use crate::cx::Cx;
use bytes::Bytes;
use redis_proto::Reply;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

/// A command error: the full error line including its code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub Cow<'static, str>);

impl Error {
    pub const fn new(s: &'static str) -> Self {
        Error(Cow::Borrowed(s))
    }
    pub fn msg(s: impl Into<String>) -> Self {
        Error(Cow::Owned(s.into()))
    }
}

impl From<Error> for Reply {
    fn from(e: Error) -> Reply {
        Reply::Error(e.0.into_owned())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type CmdResult = Result<Reply, Error>;
pub type Handler = fn(&mut Cx, &[Bytes]) -> CmdResult;

pub const WRONGTYPE: Error = Error::new("WRONGTYPE Operation against a key holding the wrong kind of value");
pub const NOT_INT: Error = Error::new("ERR value is not an integer or out of range");
pub const NOT_FLOAT: Error = Error::new("ERR value is not a valid float");
pub const SYNTAX: Error = Error::new("ERR syntax error");
pub const NO_KEY: Error = Error::new("ERR no such key");
pub const OVERFLOW: Error = Error::new("ERR increment or decrement would overflow");
pub const OUT_OF_RANGE: Error = Error::new("ERR index out of range");
pub const OOM: Error = Error::new("OOM command not allowed when used memory > 'maxmemory'.");

pub fn arity_error(name: &str) -> Error {
    Error::msg(format!("ERR wrong number of arguments for '{name}' command"))
}

// ---- flags ----
pub const WRITE: u64 = 1 << 0;
pub const READONLY: u64 = 1 << 1;
pub const DENYOOM: u64 = 1 << 2;
pub const ADMIN: u64 = 1 << 3;
pub const PUBSUB: u64 = 1 << 4;
pub const NOSCRIPT: u64 = 1 << 5;
pub const BLOCKING: u64 = 1 << 6;
pub const LOADING: u64 = 1 << 7;
pub const STALE: u64 = 1 << 8;
pub const SKIP_MONITOR: u64 = 1 << 9;
pub const SKIP_SLOWLOG: u64 = 1 << 10;
pub const FAST: u64 = 1 << 11;
pub const NO_AUTH: u64 = 1 << 12;
pub const MAY_REPLICATE: u64 = 1 << 13;
pub const NO_MULTI: u64 = 1 << 14;
pub const ALLOW_BUSY: u64 = 1 << 15;
pub const NO_MANDATORY_KEYS: u64 = 1 << 16;
pub const NO_ASYNC_LOADING: u64 = 1 << 17;
pub const MOVABLE_KEYS: u64 = 1 << 18;
pub const DANGEROUS: u64 = 1 << 19;
/// Allowed while a RESP2 client is in subscribed mode.
pub const SUBSCRIBED_OK: u64 = 1 << 20;
/// Executed by the server layer (connection/tenant state), not by a shard engine.
pub const CONN: u64 = 1 << 21;
/// Touches every shard (keyless writes like FLUSHALL, aggregate reads like DBSIZE).
pub const ALL_SHARDS: u64 = 1 << 22;
/// Queued normally inside MULTI is not allowed: EXEC, DISCARD, MULTI, WATCH, QUIT, RESET run
/// immediately.
pub const TXN_CONTROL: u64 = 1 << 23;
/// Runs scripts or functions (EVAL, FCALL…).
pub const SCRIPT_RUNNER: u64 = 1 << 24;
/// Allowed on a cluster node that is importing the slot (`ASKING`).
pub const ASKING: u64 = 1 << 25;
/// Implemented by a module in Redis (JSON, Bloom, Search…).
pub const MODULE: u64 = 1 << 26;
/// Not a command flag: a module command its module puts in the @fast ACL category.
pub const ACL_FAST: u64 = 1 << 27;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Generic,
    String,
    List,
    Set,
    SortedSet,
    Hash,
    PubSub,
    Transactions,
    Connection,
    Server,
    Scripting,
    HyperLogLog,
    Cluster,
    Geo,
    Stream,
    Bitmap,
    Array,
    Module,
}

impl Group {
    pub fn name(self) -> &'static str {
        match self {
            Group::Generic => "generic",
            Group::String => "string",
            Group::List => "list",
            Group::Set => "set",
            Group::SortedSet => "sorted-set",
            Group::Hash => "hash",
            Group::PubSub => "pubsub",
            Group::Transactions => "transactions",
            Group::Connection => "connection",
            Group::Server => "server",
            Group::Scripting => "scripting",
            Group::HyperLogLog => "hyperloglog",
            Group::Cluster => "cluster",
            Group::Geo => "geo",
            Group::Stream => "stream",
            Group::Bitmap => "bitmap",
            Group::Array => "array",
            Group::Module => "module",
        }
    }
}

/// Key positions for a command whose keys can't be described by first/last/step.
pub type KeysFn = fn(&[Bytes]) -> Vec<usize>;

pub struct Command {
    /// Lowercase name (`"get"`, or `"get"` under a container's `subs` for `config|get`).
    pub name: &'static str,
    /// Exact count when positive; minimum (`-n`) when negative. Includes the command name.
    pub arity: i32,
    pub flags: u64,
    pub first_key: i32,
    /// Negative counts from the end (-1 = last argument).
    pub last_key: i32,
    pub key_step: i32,
    pub keys_fn: Option<KeysFn>,
    pub handler: Handler,
    pub group: Group,
    pub since: &'static str,
    pub summary: &'static str,
    pub subs: &'static [Command],
}

impl Command {
    pub fn has(&self, f: u64) -> bool {
        self.flags & f != 0
    }

    pub fn is_write(&self) -> bool {
        self.has(WRITE)
    }

    /// Indexes of key arguments in `args`.
    pub fn key_positions(&self, args: &[Bytes]) -> Vec<usize> {
        if let Some(f) = self.keys_fn {
            return f(args).into_iter().filter(|&i| i < args.len()).collect();
        }
        if self.first_key <= 0 {
            return Vec::new();
        }
        let n = args.len() as i32;
        let last = if self.last_key < 0 { n + self.last_key } else { self.last_key };
        let step = self.key_step.max(1);
        let mut out = Vec::new();
        let mut i = self.first_key;
        while i <= last && i < n {
            out.push(i as usize);
            i += step;
        }
        out
    }

    pub fn keys<'a>(&self, args: &'a [Bytes]) -> Vec<&'a Bytes> {
        self.key_positions(args).into_iter().map(|i| &args[i]).collect()
    }

    pub fn check_arity(&self, n: usize) -> bool {
        let n = n as i32;
        if self.arity >= 0 { n == self.arity } else { n >= -self.arity }
    }

    /// ACL categories (`@read`, `@string`, …), in Redis' order: Redis' own table for the commands
    /// it has, otherwise derived from flags and group.
    pub fn categories(&self) -> Vec<&'static str> {
        crate::acl::categories_of(self)
    }

    /// `COMMAND INFO`-style flag names.
    pub fn flag_names(&self) -> Vec<&'static str> {
        // Redis' order; may_replicate is hidden on purpose, as there.
        let names = [
            (WRITE, "write"),
            (READONLY, "readonly"),
            (DENYOOM, "denyoom"),
            (MODULE, "module"),
            (ADMIN, "admin"),
            (PUBSUB, "pubsub"),
            (NOSCRIPT, "noscript"),
            (BLOCKING, "blocking"),
            (LOADING, "loading"),
            (STALE, "stale"),
            (SKIP_MONITOR, "skip_monitor"),
            (SKIP_SLOWLOG, "skip_slowlog"),
            (ASKING, "asking"),
            (FAST, "fast"),
            (NO_AUTH, "no_auth"),
            (NO_MANDATORY_KEYS, "no_mandatory_keys"),
            (NO_ASYNC_LOADING, "no_async_loading"),
            (NO_MULTI, "no_multi"),
            (MOVABLE_KEYS, "movablekeys"),
            (ALLOW_BUSY, "allow_busy"),
            (SCRIPT_RUNNER, "script_runner"),
        ];
        names.iter().filter(|(f, _)| self.has(*f)).map(|(_, n)| *n).collect()
    }
}

/// Shorthand for table entries:
/// `cmd!("get", 2, READONLY | FAST, 1, 1, 1, get, String)`.
#[macro_export]
macro_rules! cmd {
    ($name:literal, $arity:expr, $flags:expr, $first:expr, $last:expr, $step:expr, $handler:expr, $group:ident) => {
        $crate::commands::Command {
            name: $name,
            arity: $arity,
            flags: $flags,
            first_key: $first,
            last_key: $last,
            key_step: $step,
            keys_fn: None,
            handler: $handler,
            group: $crate::commands::Group::$group,
            since: "1.0.0",
            summary: "",
            subs: &[],
        }
    };
    ($name:literal, $arity:expr, $flags:expr, $first:expr, $last:expr, $step:expr, keys = $kf:expr, $handler:expr, $group:ident) => {
        $crate::commands::Command {
            first_key: $first,
            last_key: $last,
            key_step: $step,
            ..$crate::cmd!($name, $arity, $flags, keys = $kf, $handler, $group)
        }
    };
    ($name:literal, $arity:expr, $flags:expr, keys = $kf:expr, $handler:expr, $group:ident) => {
        $crate::commands::Command {
            name: $name,
            arity: $arity,
            flags: $flags | $crate::commands::MOVABLE_KEYS,
            first_key: 0,
            last_key: 0,
            key_step: 0,
            keys_fn: Some($kf),
            handler: $handler,
            group: $crate::commands::Group::$group,
            since: "1.0.0",
            summary: "",
            subs: &[],
        }
    };
}

/// A container command (`CONFIG`, `OBJECT`, `XINFO`…) dispatching to `subs`.
#[macro_export]
macro_rules! container {
    ($name:literal, $flags:expr, $group:ident, $subs:expr) => {
        $crate::commands::Command {
            name: $name,
            arity: -2,
            flags: $flags,
            first_key: 0,
            last_key: 0,
            key_step: 0,
            keys_fn: None,
            handler: $crate::commands::no_handler,
            group: $crate::commands::Group::$group,
            since: "1.0.0",
            summary: "",
            subs: $subs,
        }
    };
}

pub fn no_handler(_: &mut Cx, _: &[Bytes]) -> CmdResult {
    Err(Error::new("ERR unknown subcommand"))
}

/// Handler for commands the server layer intercepts; reached only when the engine is embedded
/// without a server.
pub fn server_only(_: &mut Cx, args: &[Bytes]) -> CmdResult {
    Err(Error::msg(format!("ERR '{}' is not available in this context", String::from_utf8_lossy(&args[0]).to_lowercase())))
}

pub struct Table {
    pub by_name: HashMap<&'static str, &'static Command>,
    pub all: Vec<&'static Command>,
}

static TABLE: OnceLock<Table> = OnceLock::new();

pub fn table() -> &'static Table {
    TABLE.get_or_init(|| {
        let mut by_name = HashMap::new();
        let mut all = Vec::new();
        for family in [
            strings::COMMANDS,
            keys::COMMANDS,
            server::COMMANDS,
            acl::COMMANDS,
            multi::COMMANDS,
            pubsub::COMMANDS,
            lists::COMMANDS,
            hashes::COMMANDS,
            sets::COMMANDS,
            sort::COMMANDS,
            zsets::COMMANDS,
            streams::COMMANDS,
            arrays::COMMANDS,
            bitmaps::COMMANDS,
            hll::COMMANDS,
            geo::COMMANDS,
            scripting::COMMANDS,
            migrate::COMMANDS,
            hotkeys::COMMANDS,
            json::COMMANDS,
            bloom::COMMANDS,
            sketch::COMMANDS,
        ] {
            for c in family {
                assert!(by_name.insert(c.name, c).is_none(), "duplicate command {}", c.name);
                all.push(c);
            }
        }
        Table { by_name, all }
    })
}

/// Look up a top-level command by (any-case) name.
pub fn lookup(name: &[u8]) -> Option<&'static Command> {
    if name.len() > 32 {
        return None;
    }
    let mut buf = [0u8; 32];
    let lower = &mut buf[..name.len()];
    lower.copy_from_slice(name);
    lower.make_ascii_lowercase();
    table().by_name.get(std::str::from_utf8(lower).ok()?).copied()
}

/// What CLIENT LIST shows as the command of `args` (Redis' `lastcmd`, set before checks):
/// the full name, or `NULL` for an unknown command or subcommand.
pub fn last_cmd_name(args: &[Bytes]) -> &'static str {
    let Some(cmd) = lookup(&args[0]) else { return "NULL" };
    let cmd = if cmd.subs.is_empty() || args.len() < 2 {
        cmd
    } else {
        match lookup_sub(cmd, &args[1]) {
            Some(s) => s,
            None => return "NULL",
        }
    };
    crate::acl::full_name(cmd).unwrap_or(cmd.name)
}

/// Resolve a container's subcommand.
pub fn lookup_sub(cmd: &'static Command, sub: &[u8]) -> Option<&'static Command> {
    cmd.subs.iter().find(|s| s.name.as_bytes().eq_ignore_ascii_case(sub))
}

// ---- argument helpers ----

pub fn int(a: &[u8]) -> Result<i64, Error> {
    crate::util::parse_i64(a).ok_or(NOT_INT)
}

/// A non-negative integer; negative values get `msg`.
pub fn uint(a: &[u8], msg: &'static str) -> Result<u64, Error> {
    let v = int(a)?;
    if v < 0 { Err(Error::new(msg)) } else { Ok(v as u64) }
}

pub fn float(a: &[u8]) -> Result<f64, Error> {
    redis_proto::parse_double(a).ok_or(NOT_FLOAT)
}

#[inline]
pub fn eq(a: &[u8], s: &str) -> bool {
    a.eq_ignore_ascii_case(s.as_bytes())
}

/// Parse a timeout in seconds (float) for blocking commands into milliseconds; 0 = forever.
pub fn timeout_secs(a: &[u8]) -> Result<i64, Error> {
    let f = redis_proto::parse_double(a).ok_or(Error::new("ERR timeout is not a float or out of range"))?;
    let ms = f * 1000.0;
    // Redis truncates to whole ms first, so -0.0005 means "forever", not negative.
    if ms <= -1.0 {
        return Err(Error::new("ERR timeout is negative"));
    }
    if !ms.is_finite() || ms > (i64::MAX / 2) as f64 {
        return Err(Error::new("ERR timeout is out of range"));
    }
    Ok(ms.ceil() as i64)
}

/// Normalise a `[start, end]` range with negative indexes against `len`; `None` if empty.
pub fn range(start: i64, end: i64, len: usize) -> Option<(usize, usize)> {
    let len = len as i64;
    let mut s = if start < 0 { len + start } else { start };
    let mut e = if end < 0 { len + end } else { end };
    if s < 0 {
        s = 0;
    }
    if s > e || s >= len {
        return None;
    }
    if e >= len {
        e = len - 1;
    }
    Some((s as usize, e as usize))
}

/// `unknown subcommand` error in Redis' wording.
pub fn unknown_sub(cmd: &str, sub: &[u8]) -> Error {
    Error::msg(format!(
        "ERR unknown subcommand '{}'. Try {} HELP.",
        String::from_utf8_lossy(sub),
        cmd.to_uppercase()
    ))
}

/// A `HELP` reply.
pub fn help(lines: &[&str]) -> Reply {
    Reply::Array(lines.iter().map(|l| Reply::Status(l.to_string())).collect())
}
