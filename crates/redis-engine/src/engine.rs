//! The engine: one shard's keyspace (16 databases) and everything needed to execute commands
//! against it with Redis semantics: dispatch and arity, MULTI queueing, WATCH, blocking
//! operations, lazy and active expiry, maxmemory eviction, keyspace notifications and
//! replication effects.
//!
//! The engine is synchronous and single-threaded by design (Redis' atomicity model). The server
//! wraps each shard's engine in a mutex and runs pipelined batches under one lock acquisition.

use crate::client::Client;
use crate::commands::{self, Command, Error};
use crate::config::{Config, Policy};
use crate::cx::Cx;
use crate::db::Db;
use crate::hub::Hub;
use crate::notify;
use crate::util::{Rng, now_ms};
use crate::value::{Entry, Str, Value};
use bytes::Bytes;
use redis_proto::Reply;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

/// A write to replicate / persist, in deterministic form (relative TTLs made absolute, random
/// choices resolved, scripts expanded into the commands they ran).
#[derive(Clone, Debug, PartialEq)]
pub struct Effect {
    pub db: u16,
    pub args: Vec<Bytes>,
}

pub enum Outcome {
    Reply(Reply),
    /// The client is now blocked; its reply arrives later through its sink.
    Blocked,
}

impl Outcome {
    pub fn unwrap_reply(self) -> Reply {
        match self {
            Outcome::Reply(r) => r,
            Outcome::Blocked => panic!("command blocked"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A command from a client.
    Top,
    /// Inside EXEC.
    Exec,
    /// From a Lua script or function (`read_only` for EVAL_RO / no-writes scripts).
    Script { read_only: bool },
    /// Applying a replication / log effect.
    Replay,
    /// Re-running a blocked command because one of its keys changed.
    Unblock,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub commands: u64,
    pub keyspace_hits: u64,
    pub keyspace_misses: u64,
    pub expired_keys: u64,
    pub expired_stale_perc: f64,
    pub expired_time_cap_reached: u64,
    pub evicted_keys: u64,
    pub evicted_clients: u64,
    pub reply_buffer_shrinks: u64,
    pub reply_buffer_expands: u64,
    pub evicted_scripts: u64,
    pub total_eviction_exceeded_time_ms: u64,
    pub rejected_calls: u64,
    pub failed_calls: u64,
    pub unexpected_error_replies: u64,
    pub total_blocking_keys: u64,
    /// Keys deleted by the active-expiry cycle (also counted in `expired_keys`).
    pub expired_keys_active: u64,
    /// Hash fields expired, and those expired by the active cycle.
    pub expired_subkeys: u64,
    pub expired_subkeys_active: u64,
    /// Microseconds spent in active expiry.
    pub expire_cycle_us: u64,
    /// Commands that went to the slowlog, their total and longest duration (µs).
    pub slowlog_count: u64,
    pub slowlog_us_sum: u64,
    pub slowlog_us_max: u64,
    /// SAVE / BGSAVE runs.
    pub rdb_saves: u64,
    /// Error replies sent (`total_error_replies`).
    pub error_replies: u64,
}

#[derive(Clone, Debug, Default)]
pub struct CmdStat {
    pub calls: u64,
    pub usec: u64,
    pub rejected: u64,
    pub failed: u64,
    pub slowlog_count: u64,
    pub slowlog_us_sum: u64,
    pub slowlog_us_max: u64,
    /// Latency distribution (INFO latencystats), while `latency-tracking` is on.
    pub latency: Option<Box<crate::commands::info::Histogram>>,
}

#[derive(Clone, Debug)]
pub struct SlowEntry {
    pub id: u64,
    pub at: i64,
    pub micros: u64,
    pub args: Vec<Bytes>,
    pub addr: String,
    pub name: String,
    /// The command's full argument count.
    pub argc: usize,
}

#[derive(Default)]
pub(crate) struct Signals {
    /// Record modified keys (someone watches, blocks or tracks).
    pub track: bool,
    pub touched: Vec<(u16, Str)>,
    /// Aggregates that may have become empty and must be deleted.
    pub empty: Vec<(u16, Str)>,
    /// Keyspace notifications to publish: (class, event, db, key, subkeys).
    pub events: Vec<(u32, &'static str, u16, Str, Vec<Bytes>)>,
    pub notify: u32,
    /// Changes made (Redis' `server.dirty`); drives propagation.
    pub dirty: u64,
    /// The running command's own count of changes, replacing its touches (`server.dirty += n`).
    pub counted: Option<u64>,
    /// RedisTimeSeries subscribed to keyspace events (its first series was made): the events it
    /// listens to are queued for it whatever `notify-keyspace-events` says.
    pub ts_hook: bool,
    pub ts_events: Vec<(&'static str, u16, Str)>,
    /// RediSearch subscribed to keyspace events (its first index was created): the events of
    /// its classes are queued for it.
    pub search_hook: bool,
    pub search_events: Vec<(&'static str, u16, Str)>,
}

impl Signals {
    #[inline]
    pub fn notify(&mut self, class: u32, event: &'static str, db: u16, key: &[u8]) {
        if self.ts_hook && crate::ts::hooked(event) {
            self.ts_events.push((event, db, Str::from_slice(key)));
        }
        if self.search_hook && class & crate::search::global::EVENT_CLASSES != 0 {
            self.search_event(event, db, key);
        }
        if self.notify & class != 0 && self.notify & (notify::KEYSPACE | notify::KEYEVENT) != 0 {
            self.events.push((class, event, db, Str::from_slice(key), Vec::new()));
        }
    }

    /// Queue an event for RediSearch's handler. Modules are called before the event is published,
    /// so where the handler opens the key with effects (a `keymiss`, the hit/miss stats) a probe
    /// holds the place ahead of the event.
    fn search_event(&mut self, event: &'static str, db: u16, key: &[u8]) {
        if crate::commands::ft::probes_key(event) {
            self.events.push((notify::SEARCH_PROBE, "", 0, Str::from_slice(key), Vec::new()));
        }
        self.search_events.push((event, db, Str::from_slice(key)));
    }

    /// An event about some of the key's fields (`notifyKeyspaceEventWithSubkeys`): also published
    /// on the subkey channels when those are enabled.
    pub fn notify_fields<F: AsRef<[u8]>>(&mut self, class: u32, event: &'static str, db: u16, key: &[u8], fields: impl IntoIterator<Item = F>) {
        if self.search_hook && class & crate::search::global::EVENT_CLASSES != 0 {
            self.search_event(event, db, key);
        }
        if self.notify & class == 0 || self.notify & (notify::KEYSPACE | notify::KEYEVENT | notify::SUBKEY_ALL) == 0 {
            return;
        }
        let subkeys = if self.notify & notify::SUBKEY_ALL != 0 {
            fields.into_iter().map(|f| Bytes::copy_from_slice(f.as_ref())).collect()
        } else {
            Vec::new()
        };
        self.events.push((class, event, db, Str::from_slice(key), subkeys));
    }
    #[inline]
    pub fn touch(&mut self, db: u16, key: &[u8]) {
        self.dirty += 1;
        self.touch_quiet(db, key);
    }

    /// Signal a modified key without counting a change: expiry, eviction and the deletion of an
    /// emptied aggregate aren't changes of their own in Redis.
    #[inline]
    pub fn touch_quiet(&mut self, db: u16, key: &[u8]) {
        if self.track {
            self.touched.push((db, Str::from_slice(key)));
        }
    }
}

#[derive(Default)]
pub(crate) struct Watches {
    pub keys: HashMap<(u16, Str), HashSet<u64>>,
    /// Per client: watched keys and whether each was missing/expired when watched.
    pub by_client: HashMap<u64, Vec<(u16, Str, bool)>>,
    pub dirty: HashSet<u64>,
}

pub(crate) struct Waiter {
    pub client: Client,
    pub args: Vec<Bytes>,
    pub keys: Vec<(u16, Str)>,
    pub timeout_reply: Reply,
    /// Absolute Unix ms; 0 = forever.
    pub deadline: i64,
    /// Also re-run when the key is deleted (XREADGROUP: the group is gone).
    pub unblock_on_nokey: bool,
    /// Re-run at this absolute Unix ms even if no key changed (XREADGROUP CLAIM: a pending
    /// entry becomes claimable); 0 = never.
    pub wake_at: i64,
    /// Execution time of the attempts so far: Redis charges it with the call that ends the block.
    pub micros: u64,
    /// A module block: a timeout (or CLIENT UNBLOCK) re-runs the command to answer instead of
    /// sending `timeout_reply` ([`Engine::timing_out`] is set meanwhile).
    pub rerun_on_timeout: bool,
}

#[derive(Default)]
pub(crate) struct Blocking {
    pub by_key: HashMap<(u16, Str), VecDeque<u64>>,
    pub waiters: HashMap<u64, Waiter>,
    pub ready: Vec<(u16, Str)>,
    pub ready_set: HashSet<(u16, Str)>,
    /// A blocked command is being re-run because its block timed out.
    pub timing_out: bool,
}

/// What a handler asks for when it can't complete yet.
pub(crate) struct BlockReq {
    pub keys: Vec<Str>,
    pub timeout_reply: Reply,
    pub deadline: i64,
    /// Arguments to re-run with instead of the original ones (XREAD rewrites `$`).
    pub args: Option<Vec<Bytes>>,
    pub unblock_on_nokey: bool,
    pub wake_at: i64,
    pub rerun_on_timeout: bool,
}

pub struct Engine {
    pub dbs: Vec<Db>,
    pub cfg: Config,
    pub hub: Arc<Hub>,
    /// `server.cmd_time_snapshot`: the time the current execution unit (the outermost command,
    /// or a cron job like active expiry) runs at; what module keyspace handlers see as now.
    pub(crate) exec_time: i64,
    /// `server.execution_nesting`: dispatches in progress (nested ones keep the snapshot).
    exec_depth: u32,
    pub stats: Stats,
    pub cmdstats: HashMap<String, CmdStat>,
    pub errorstats: HashMap<String, u64>,
    pub slowlog: VecDeque<SlowEntry>,
    pub slowlog_next_id: u64,
    pub rng: Rng,
    /// Read-only replica: writes are rejected and keys are expired only by the primary's DELs.
    pub replica: bool,
    /// `sig.dirty` at the last snapshot: changes since are `rdb_changes_since_last_save`.
    pub dirty_mark: u64,
    pub lastsave: i64,
    /// SAVE / BGSAVE / BGREWRITEAOF bookkeeping.
    pub persist: crate::commands::persist::Persist,
    pub start_ms: i64,
    /// Effects of executed writes, drained by the server into the replication log.
    pub effects: Vec<Effect>,
    pub scripting: crate::commands::scripting::Scripting,
    pub(crate) sig: Signals,
    pub(crate) watches: Watches,
    pub(crate) blocking: Blocking,
    pub(crate) evict_pool: Vec<(u64, u16, Str)>,
    /// Extra bytes outside the keyspace counted towards maxmemory (scripts, buffers).
    pub overhead: usize,
    /// Shard index within its database, for INFO and SCAN cursors.
    pub shard: u16,
    /// The database's ACL users and log, shared by all of its shards.
    pub acl: Arc<parking_lot::RwLock<crate::acl::Acl>>,
    /// Over maxmemory when the current top-level command started (`pre_command_oom_state`):
    /// scripts check it before their first write.
    pub(crate) pre_oom: bool,
    /// MIGRATE's cached target connections.
    pub(crate) migrate_sockets: crate::commands::migrate::Sockets,
    /// Connected clients, CLIENT PAUSE and client-side caching.
    pub clients: crate::clients::Clients,
    /// The HOTKEYS session, if one was started and not reset.
    pub hotkeys: Option<crate::commands::hotkeys::Stats>,
    /// Reply bytes of the EXEC running now, so far (Redis counts them per queued command).
    pub(crate) exec_out_bytes: u64,
    /// What the server around the engine reports for INFO (connections, network, replication).
    pub host: Option<Arc<dyn crate::commands::info::Host>>,
    /// Work for the server after the current command (REPLICAOF), taken with
    /// [`Engine::take_server_actions`].
    pub server_actions: Vec<crate::commands::replication::ServerAction>,
    /// INFO memory: the peak of `used_memory` and when it was reached (Unix s), and the usage
    /// right after startup.
    pub peak_memory: usize,
    pub peak_time: i64,
    pub startup_memory: usize,
    /// INFO `instantaneous_*` samples.
    pub metrics: crate::commands::info::Metrics,
    /// `total_error_replies` when the current command started or its last nested call ended.
    pub(crate) prev_errors: u64,
    /// DEBUG's switches.
    pub debug: crate::commands::debug::Switches,
    /// RedisTimeSeries' global state: the label index (over every database).
    pub ts: crate::ts::Global,
    /// RediSearch's global state: the indexes, their prefixes, aliases and dictionaries.
    pub search: crate::search::global::Global,
}

/// The arguments as SLOWLOG and MONITOR show them: secrets replaced by `(redacted)`, like Redis'
/// `redactClientCommandArgument` (passwords, ACL rules, sensitive configs).
pub(crate) fn redacted<'a>(full: &str, args: &'a [Bytes]) -> std::borrow::Cow<'a, [Bytes]> {
    let hide: Vec<usize> = match full {
        "auth" => (1..args.len().min(3)).collect(),
        "acl|setuser" | "acl|deluser" => (2..args.len()).collect(),
        "acl|getuser" => vec![2],
        "config|set" => (2..args.len())
            .step_by(2)
            .filter(|&i| i + 1 < args.len() && crate::config::is_sensitive(&args[i]))
            .map(|i| i + 1)
            .collect(),
        "hello" => {
            let mut v = Vec::new();
            let mut j = 2;
            while j < args.len() {
                if args[j].eq_ignore_ascii_case(b"auth") && j + 2 < args.len() {
                    v.extend([j + 1, j + 2]);
                    j += 3;
                } else {
                    j += 1;
                }
            }
            v
        }
        "migrate" => {
            let mut v = Vec::new();
            let mut j = 6;
            while j < args.len() {
                if args[j].eq_ignore_ascii_case(b"auth") {
                    v.push(j + 1);
                    j += 2;
                } else if args[j].eq_ignore_ascii_case(b"auth2") {
                    v.extend([j + 1, j + 2]);
                    j += 3;
                } else if args[j].eq_ignore_ascii_case(b"keys") {
                    break;
                } else {
                    j += 1;
                }
            }
            v
        }
        _ => return std::borrow::Cow::Borrowed(args),
    };
    let mut out = args.to_vec();
    for i in hide {
        if let Some(a) = out.get_mut(i) {
            *a = Bytes::from_static(b"(redacted)");
        }
    }
    std::borrow::Cow::Owned(out)
}

impl Engine {
    pub fn new(cfg: Config, hub: Arc<Hub>) -> Self {
        let dbs = (0..cfg.databases).map(|_| Db::default()).collect();
        let now = now_ms();
        // `srand(time(NULL) ^ getpid())` at startup: top-k decay draws from it.
        crate::bloom::topk::crand::srand(((now / 1000) as u32) ^ std::process::id());
        let mut e = Self {
            dbs,
            cfg,
            hub,
            exec_time: now,
            exec_depth: 0,
            stats: Stats::default(),
            cmdstats: HashMap::new(),
            errorstats: HashMap::new(),
            slowlog: VecDeque::new(),
            slowlog_next_id: 0,
            rng: Rng::new(),
            replica: false,
            dirty_mark: 0,
            lastsave: now / 1000,
            persist: Default::default(),
            start_ms: now,
            effects: Vec::new(),
            scripting: Default::default(),
            sig: Signals::default(),
            watches: Watches::default(),
            blocking: Blocking::default(),
            evict_pool: Vec::new(),
            overhead: 0,
            shard: 0,
            acl: Default::default(),
            pre_oom: false,
            migrate_sockets: Default::default(),
            clients: Default::default(),
            hotkeys: None,
            exec_out_bytes: 0,
            host: None,
            server_actions: Vec::new(),
            peak_memory: 0,
            peak_time: now / 1000,
            startup_memory: 0,
            metrics: Default::default(),
            prev_errors: 0,
            debug: Default::default(),
            ts: Default::default(),
            search: Default::default(),
        };
        e.startup_memory = e.used_memory();
        e.peak_memory = e.startup_memory;
        e.hub.set_limits(&e.cfg);
        if e.cfg.appendonly {
            e.open_aof_at_start();
        }
        e
    }

    /// Changes since the last snapshot (`rdb_changes_since_last_save`).
    pub fn dirty(&self) -> u64 {
        self.sig.dirty.saturating_sub(self.dirty_mark)
    }

    /// Restart the count of changes since the last snapshot (a save succeeded).
    pub fn reset_dirty(&mut self) {
        self.dirty_mark = self.sig.dirty;
    }

    /// A standalone engine with default config, for tests and tools.
    pub fn standalone() -> Self {
        Self::new(Config::default(), Hub::new())
    }

    /// Execute one client command.
    pub fn call(&mut self, client: &mut Client, args: &[Bytes]) -> Outcome {
        let now = now_ms();
        // Connections register on connect when the engine is free, else on their first command.
        if client.conn.is_some() && !self.clients.by_id.contains_key(&client.id) {
            self.clients.register(client);
        }
        if let Some(c) = &client.conn {
            c.last_cmd_ms.store(now, std::sync::atomic::Ordering::Relaxed);
        }
        if self.clients.tracking_clients > 0 {
            self.clients.limit(self.cfg.tracking_table_max_keys, &mut self.rng);
        }
        self.clients.current = client.id;
        self.clients.defer_current = client.defer_pushes;
        let out = self.dispatch(client, args, now, Mode::Top);
        // The client's own invalidations follow its reply.
        self.clients.flush_pending();
        self.clients.refresh(client);
        self.clients.defer_current = false;
        self.serve_blocked(now);
        self.clients.current = 0;
        out
    }

    /// Pushes for the client of the last [`Engine::call`] that go after its reply (when it set
    /// [`Client::defer_pushes`]).
    pub fn take_after_reply(&mut self) -> Vec<Bytes> {
        self.clients.take_after_reply()
    }

    /// Work Redis does before the event loop sleeps: BCAST invalidations of the keys modified
    /// meanwhile. Call after each batch of commands and from the cron.
    pub fn before_sleep(&mut self) {
        self.clients.broadcast();
    }

    /// Would this command propagate writes (`is_may_replicate_command`): write and
    /// may-replicate commands, scripts unless they declare `no-writes`, EXEC of such commands.
    pub fn may_replicate(&self, client: &Client, args: &[Bytes]) -> bool {
        let Ok((cmd, _)) = Self::resolve_as(args, client.internal) else { return false };
        let own = cmd.is_write() || cmd.has(commands::MAY_REPLICATE);
        if cmd.has(commands::SCRIPT_RUNNER) {
            // `evalGetCommandFlags`: a script without declared flags (or not found) keeps the
            // command's own (EVAL may replicate, EVAL_RO doesn't); declared flags only add
            // `write` unless `no-writes` (`scriptFlagsToCmdFlags`).
            return match self.scripting.flags_for(args) {
                Some(f) if f & commands::scripting::COMPAT == 0 => own || f & commands::scripting::NO_WRITES == 0,
                _ => own,
            };
        }
        if cmd.name == "exec" {
            return client.multi.as_ref().is_some_and(|m| m.queued.iter().any(|q| self.may_replicate(client, q)));
        }
        own
    }

    /// CLIENT PAUSE: until when (Unix ms) this command must wait, if it is paused.
    /// A postponed command already shows as the client's current one.
    pub fn paused_for(&mut self, client: &mut Client, args: &[Bytes]) -> Option<i64> {
        if client.internal || self.clients.pause.is_none() {
            return None;
        }
        let may_replicate = self.may_replicate(client, args);
        let end = self.clients.paused(now_ms(), may_replicate)?;
        client.last_cmd = commands::last_cmd_name(args);
        self.clients.refresh(client);
        Some(end)
    }

    /// Paused clients also pause expiry and eviction.
    pub(crate) fn pause_active(&self, now: i64) -> bool {
        self.clients.pause.is_some_and(|(end, _)| end > now)
    }

    /// Register a network client (CLIENT LIST, KILL, tracking…).
    pub fn register_client(&mut self, client: &Client) {
        self.clients.register(client);
    }

    /// Apply a replicated effect (from the log or a primary).
    pub fn apply(&mut self, effect: &Effect) -> Reply {
        let mut c = Client::new(0);
        c.internal = true;
        c.db = effect.db as usize;
        let now = now_ms();
        let out = self.dispatch(&mut c, &effect.args, now, Mode::Replay);
        self.serve_blocked(now);
        match out {
            Outcome::Reply(r) => r,
            Outcome::Blocked => Reply::Nil,
        }
    }

    /// Apply a batch of effects. MULTI/EXEC markers in the log are honoured (they just apply in
    /// order, since nothing else interleaves while the batch holds the shard).
    pub fn apply_all(&mut self, effects: &[Effect]) {
        for e in effects {
            if e.args.len() == 1 && (commands::eq(&e.args[0], "multi") || commands::eq(&e.args[0], "exec")) {
                continue;
            }
            self.apply(e);
        }
    }

    pub fn used_memory(&self) -> usize {
        self.dbs.iter().map(|d| d.used_bytes()).sum::<usize>() + self.overhead + self.scripting.memory() + self.scripting.functions_memory()
    }

    pub fn total_keys(&self) -> usize {
        self.dbs.iter().map(|d| d.len()).sum()
    }

    pub fn take_server_actions(&mut self) -> Vec<crate::commands::replication::ServerAction> {
        std::mem::take(&mut self.server_actions)
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    fn unknown_command(args: &[Bytes]) -> Reply {
        let mut tail = String::new();
        for a in &args[1..] {
            if tail.len() >= 128 {
                break;
            }
            let s = String::from_utf8_lossy(a);
            let room = 128 - tail.len();
            tail.push_str(&format!("'{}' ", s.chars().take(room).collect::<String>()));
        }
        let name = String::from_utf8_lossy(&args[0]).chars().take(128).collect::<String>();
        if args.len() == 1 {
            return Reply::Error(format!("ERR unknown command '{name}'"));
        }
        Reply::Error(format!("ERR unknown command '{name}', with args beginning with: {tail}"))
    }

    /// Resolve the command (and subcommand) for `args`, checking arity.
    pub fn resolve(args: &[Bytes]) -> Result<(&'static Command, String), Reply> {
        Self::resolve_as(args, false)
    }

    /// [`Engine::resolve`] for a connection that may (`internal`) or may not see internal
    /// commands. An internal container's subcommands are visible to all (as in Redis, which
    /// checks the resolved command).
    pub fn resolve_as(args: &[Bytes], internal: bool) -> Result<(&'static Command, String), Reply> {
        let Some(cmd) = commands::lookup(&args[0]) else {
            return Err(Self::unknown_command(args));
        };
        if cmd.has(commands::INTERNAL) && !internal {
            if cmd.subs.is_empty() {
                return Err(Self::unknown_command(args));
            }
            let name = String::from_utf8_lossy(&args[0]).to_uppercase();
            if args.len() < 2 {
                return Err(Reply::Error(format!("ERR missing subcommand. Try {name} HELP.")));
            }
            let Some(sub) = commands::lookup_sub(cmd, &args[1]) else {
                return Err(Reply::Error(format!(
                    "ERR unknown subcommand '{}'. Try {name} HELP.",
                    String::from_utf8_lossy(&args[1]).chars().take(128).collect::<String>()
                )));
            };
            let full = format!("{}|{}", cmd.name, sub.name);
            if !sub.check_arity(args.len()) {
                return Err(commands::arity_error(&full).into());
            }
            return Ok((sub, full));
        }
        if cmd.subs.is_empty() {
            if !cmd.check_arity(args.len()) {
                return Err(commands::arity_error(cmd.name).into());
            }
            return Ok((cmd, cmd.name.to_string()));
        }
        if args.len() < 2 {
            if cmd.check_arity(args.len()) {
                return Ok((cmd, cmd.name.to_string()));
            }
            return Err(commands::arity_error(cmd.name).into());
        }
        let Some(sub) = commands::lookup_sub(cmd, &args[1]) else {
            return Err(Reply::Error(format!(
                "ERR unknown subcommand '{}'. Try {} HELP.",
                String::from_utf8_lossy(&args[1]).chars().take(128).collect::<String>(),
                cmd.name.to_uppercase()
            )));
        };
        let full = format!("{}|{}", cmd.name, sub.name);
        if !sub.check_arity(args.len()) {
            return Err(commands::arity_error(&full).into());
        }
        Ok((sub, full))
    }

    pub(crate) fn dispatch(&mut self, client: &mut Client, args: &[Bytes], now: i64, mode: Mode) -> Outcome {
        // `enterExecutionUnit`: the outermost call takes the time snapshot.
        if self.exec_depth == 0 {
            self.exec_time = now;
        }
        self.exec_depth += 1;
        let out = self.dispatch_unit(client, args, now, mode);
        self.exec_depth -= 1;
        out
    }

    fn dispatch_unit(&mut self, client: &mut Client, args: &[Bytes], now: i64, mode: Mode) -> Outcome {
        if args.is_empty() {
            return Outcome::Reply(Reply::error("ERR empty command"));
        }
        let top = matches!(mode, Mode::Top);
        if top {
            client.last_cmd = commands::last_cmd_name(args);
        }
        // RediSearch: events from outside commands (expiry, eviction), then its command filter
        // (which runs as the command is read, so at queue time inside MULTI).
        if !self.sig.search_events.is_empty() {
            crate::commands::ft::drain(self);
        }
        if matches!(mode, Mode::Top | Mode::Script { .. } | Mode::Replay) {
            crate::commands::ft::command_filter(self, args);
        }
        let (cmd, full_name) = match Self::resolve_as(args, client.internal || mode == Mode::Replay) {
            Ok(c) => c,
            Err(e) => {
                if top && let Some(m) = client.multi.as_mut() {
                    m.error = true;
                }
                // An arity error is a rejected call of the command it names (rejectCommand).
                if let Reply::Error(t) = &e
                    && let Some(name) = t.strip_prefix("ERR wrong number of arguments for '").and_then(|r| r.strip_suffix("' command"))
                {
                    let name = name.to_string();
                    return Outcome::Reply(self.reject(&name, e));
                }
                self.count_error(&e);
                return Outcome::Reply(e);
            }
        };

        if top && !cmd.has(commands::TXN_CONTROL) && cmd.has(commands::NO_MULTI) && let Some(m) = client.multi.as_mut() {
            m.error = true;
            return Outcome::Reply(self.reject(&full_name, Reply::error("ERR Command not allowed inside a transaction")));
        }

        if client.user.is_placeholder() {
            client.user = self.acl.read().default_user();
        }
        if mode != Mode::Replay
            && let Err(d) = client.user.check(cmd, args) {
                if top && let Some(m) = client.multi.as_mut() {
                    m.error = true;
                }
                let mut e = crate::acl::deny(&self.acl, client, &full_name, args, d, mode);
                if top && full_name == "exec" {
                    // A rejected EXEC discards the transaction (Redis' execCommandAbort).
                    client.multi = None;
                    self.unwatch_all(client.id);
                    if let Reply::Error(msg) = &e {
                        e = Reply::Error(format!("EXECABORT Transaction discarded because of: {msg}"));
                    }
                }
                return Outcome::Reply(self.reject(&full_name, e));
            }

        if top && !cmd.has(commands::TXN_CONTROL) && client.multi.is_some() {
            let deny_oom = cmd.has(commands::DENYOOM) && !self.evict_to_fit(now);
            let Some(m) = client.multi.as_mut() else { unreachable!() };
            if deny_oom {
                m.error = true;
                return Outcome::Reply(self.reject(&full_name, commands::OOM.into()));
            }
            m.queued.push(args.to_vec());
            return Outcome::Reply(Reply::status("QUEUED"));
        }

        if client.resp == 2 && client.subscriptions() > 0 && !cmd.has(commands::SUBSCRIBED_OK) && mode == Mode::Top {
            return Outcome::Reply(self.reject(
                &full_name,
                Reply::Error(format!(
                    "ERR Can't execute '{full_name}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context"
                )),
            ));
        }

        if let Mode::Script { read_only } = mode {
            if cmd.has(commands::NOSCRIPT) && !self.debug.disable_deny_scripts {
                return Outcome::Reply(self.reject(&full_name, Reply::error("ERR This Redis command is not allowed from script")));
            }
            // May-replicate commands count as writes here, so read-only scripts can't publish.
            if read_only && (cmd.is_write() || cmd.has(commands::MAY_REPLICATE)) {
                return Outcome::Reply(self.reject(&full_name, Reply::error("ERR Write commands are not allowed from read-only scripts.")));
            }
        }

        if self.replica && cmd.is_write() && mode != Mode::Replay && !client.internal {
            return Outcome::Reply(self.reject(&full_name, Reply::error("READONLY You can't write against a read only replica.")));
        }

        if mode != Mode::Replay && !matches!(mode, Mode::Script { .. } | Mode::Exec) {
            self.pre_oom = false;
            if self.cfg.maxmemory > 0 {
                let fits = self.evict_to_fit(now);
                self.pre_oom = !fits;
                if !fits && cmd.has(commands::DENYOOM) && !client.internal {
                    return Outcome::Reply(self.reject(&full_name, commands::OOM.into()));
                }
            }
        }

        if matches!(mode, Mode::Script { .. })
            && let Some(run) = self.scripting.running.as_mut()
        {
            // A script can't be stopped half way, so memory is only checked before its first
            // write, and only if it was over the limit when it started.
            if !run.allow_oom && self.cfg.maxmemory > 0 && !run.write_dirty && self.pre_oom && cmd.has(commands::DENYOOM) {
                return Outcome::Reply(self.reject(&full_name, commands::OOM.into()));
            }
            if cmd.is_write() {
                run.write_dirty = true;
                self.scripting.ctl.dirty.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let monitored = top || matches!(mode, Mode::Script { .. });
        if monitored && !cmd.has(commands::SKIP_MONITOR) && !cmd.has(commands::ADMIN) && self.hub.has_monitors() {
            self.feed_monitor(client, &redacted(&full_name, args), now);
        }

        // ---- execute ----
        self.sig.notify = self.cfg.notify;
        // Always: touched keys also update the keysizes histogram.
        self.sig.track = true;
        let db_at_start = client.db as u16;
        let effects_before = self.effects.len();
        let dirty_before = self.sig.dirty;
        let outer_counted = self.sig.counted.take();
        let started = Instant::now();
        let (res, rewrite, block, no_propagate, force) = {
            // Error replies from here on (to anyone) make the command a failed call.
            self.prev_errors = self.stats.error_replies;
            let mut cx = Cx::new(self, client, now, cmd, mode);
            cx.read_stats = crate::cx::read_lookups(cmd, args);
            let res = (cmd.handler)(&mut cx, args);
            if !cx.eng.sig.ts_events.is_empty() {
                crate::commands::ts::drain(&mut cx);
            }
            if !cx.eng.sig.search_events.is_empty() {
                crate::commands::ft::drain(cx.eng);
            }
            (res, cx.rewrite.take(), cx.block.take(), cx.no_propagate, cx.force_propagate)
        };
        let micros = started.elapsed().as_micros() as u64;
        if let Some(n) = std::mem::replace(&mut self.sig.counted, outer_counted) {
            self.sig.dirty = dirty_before + n;
        }
        self.flush_empty();

        let changed = self.sig.dirty != dirty_before;
        let propagate = !matches!(mode, Mode::Script { .. }) || self.scripting.running.as_ref().is_none_or(|r| r.propagate);
        if (changed || force) && !no_propagate && propagate && mode != Mode::Replay {
            self.effects.push(Effect { db: db_at_start, args: rewrite.unwrap_or_else(|| args.to_vec()) });
        }
        // `propagatePendingCommands`: what a client's command propagated (lazily expired keys'
        // DELs and fields' HDELs included) goes out as one MULTI/EXEC when it is more than one
        // command, except for the commands that touch arbitrary keys.
        if matches!(mode, Mode::Top | Mode::Unblock) && !matches!(cmd.name, "scan" | "randomkey") {
            wrap_unit(&mut self.effects, effects_before);
        }
        self.process_touched();
        self.flush_events();

        // Client-side caching: the outer client read these keys (scripts' and transactions'
        // commands count for the client running them).
        if mode != Mode::Replay && block.is_none() {
            let current = self.clients.current;
            if current != 0
                && cmd.has(commands::READONLY)
                && !cmd.has(commands::PUBSUB)
                && !matches!(cmd.name, "eval_ro" | "evalsha_ro" | "fcall_ro")
                && self.clients.track(current).is_some()
            {
                let keys = commands::keyspec::keys_with_flags(cmd, &full_name, args);
                self.clients.remember(current, keys.into_iter().map(|(i, _)| args[i].clone()));
            }
            if block.is_none()
                && let Some(e) = self.clients.by_id.get(&current)
            {
                e.stats.cmds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if top {
                self.clients.command_done(client.id, client.multi.is_some(), full_name == "client" || full_name.starts_with("client|"));
            }
        }

        // A command that blocks is accounted for once, by the call that ends the block (served,
        // timed out or CLIENT UNBLOCKed), with the time of all its attempts.
        if let Some(b) = block {
            match mode {
                Mode::Top => {
                    self.register_waiter(client, args, b, micros);
                    return Outcome::Blocked;
                }
                Mode::Unblock => {
                    if let Some(w) = self.blocking.waiters.get_mut(&client.id) {
                        if let Some(a) = b.args {
                            w.args = a;
                        }
                        w.wake_at = b.wake_at;
                        w.micros += micros;
                    }
                    return Outcome::Blocked;
                }
                _ => {
                    self.count_call(client, &full_name, cmd, args, micros, now, mode);
                    return Outcome::Reply(b.timeout_reply);
                }
            }
        }
        let micros = if mode == Mode::Unblock { micros + self.blocking.waiters.get(&client.id).map_or(0, |w| w.micros) } else { micros };
        self.count_call(client, &full_name, cmd, args, micros, now, mode);

        let reply = match res {
            Ok(r) => r,
            Err(e) => {
                let r: Reply = e.into();
                self.count_error(&r);
                r
            }
        };
        // `incrCommandStatsOnError`: errors since the last check (nested calls check their own).
        if self.stats.error_replies > self.prev_errors {
            self.cmdstats.get_mut(&full_name).unwrap().failed += 1;
            self.stats.failed_calls += 1;
        }
        self.prev_errors = self.stats.error_replies;
        if self.hotkeys.as_ref().is_some_and(|h| h.active) && matches!(mode, Mode::Top | Mode::Exec | Mode::Unblock) {
            self.track_hotkeys(client, cmd, &full_name, args, micros, &reply, mode);
        }
        Outcome::Reply(reply)
    }

    /// Feed HOTKEYS: the command's CPU time and its request and reply bytes. Inside EXEC, Redis
    /// charges each queued command EXEC's own 14 request bytes and the transaction's reply so far.
    #[allow(clippy::too_many_arguments)]
    fn track_hotkeys(&mut self, client: &Client, cmd: &Command, full: &str, args: &[Bytes], micros: u64, reply: &Reply, mode: Mode) {
        let mut out = bytes::BytesMut::new();
        reply.encode(client.resp, &mut out);
        let bytes = if mode == Mode::Exec {
            self.exec_out_bytes += out.len() as u64;
            14 + self.exec_out_bytes
        } else {
            let mut req = bytes::BytesMut::new();
            redis_proto::encode_command(args, &mut req);
            (req.len() + out.len()) as u64
        };
        let keys: Vec<usize> = commands::keyspec::keys_with_flags(cmd, full, args).into_iter().map(|(i, _)| i).collect();
        if let Some(h) = self.hotkeys.as_mut() {
            h.track(args, &keys, micros, bytes);
        }
    }

    /// Account for a command the server layer executed itself (AUTH, WAIT…): MONITOR, SLOWLOG
    /// and command/error stats see it as if it had been dispatched here.
    pub fn record_external(&mut self, client: &Client, args: &[Bytes], micros: u64, reply: &Reply) {
        self.clients.refresh(client);
        let Ok((cmd, full)) = Self::resolve_as(args, client.internal) else { return };
        let now = now_ms();
        let shown = redacted(&full, args);
        if !cmd.has(commands::SKIP_MONITOR) && !cmd.has(commands::ADMIN) && self.hub.has_monitors() {
            self.feed_monitor(client, &shown, now);
        }
        let failed = reply.is_error();
        let stat = self.cmdstats.entry(full).or_default();
        stat.calls += 1;
        stat.usec += micros;
        if failed {
            stat.failed += 1;
            self.stats.failed_calls += 1;
            self.count_error(reply);
        }
        self.stats.commands += 1;
        if !cmd.has(commands::SKIP_SLOWLOG) {
            self.slowlog_push(client, &shown, micros, now);
        }
    }

    pub(crate) fn reject(&mut self, name: &str, r: Reply) -> Reply {
        self.cmdstats.entry(name.to_string()).or_default().rejected += 1;
        self.stats.rejected_calls += 1;
        self.count_error(&r);
        r
    }

    pub(crate) fn count_error(&mut self, r: &Reply) {
        if let Reply::Error(e) = r {
            let code = e.split(' ').next().unwrap_or("ERR").to_string();
            *self.errorstats.entry(code).or_default() += 1;
            self.stats.error_replies += 1;
        }
    }

    pub(crate) fn feed_monitor(&self, client: &Client, args: &[Bytes], now: i64) {
        let mut line = format!("{}.{:06} [{} {}]", now / 1000, (now % 1000) * 1000, client.db, client.addr);
        for a in args {
            line.push_str(" \"");
            for &c in a.iter() {
                match c {
                    b'\\' => line.push_str("\\\\"),
                    b'"' => line.push_str("\\\""),
                    b'\n' => line.push_str("\\n"),
                    b'\r' => line.push_str("\\r"),
                    b'\t' => line.push_str("\\t"),
                    7 => line.push_str("\\a"),
                    8 => line.push_str("\\b"),
                    c if c.is_ascii_graphic() || c == b' ' => line.push(c as char),
                    c => line.push_str(&format!("\\x{c:02x}")),
                }
            }
            line.push('"');
        }
        self.hub.monitor(&line);
    }

    /// Command stats for one finished call: calls and time, latency histogram, slowlog.
    #[allow(clippy::too_many_arguments)]
    fn count_call(&mut self, client: &Client, full: &str, cmd: &Command, args: &[Bytes], micros: u64, now: i64, mode: Mode) {
        let tracking = self.cfg.latency_tracking;
        let stat = self.cmdstats.entry(full.to_string()).or_default();
        stat.calls += 1;
        stat.usec += micros;
        if tracking {
            stat.latency.get_or_insert_with(Default::default).record(micros as i64 * 1000);
        }
        self.stats.commands += 1;
        if mode != Mode::Replay && !cmd.has(commands::SKIP_SLOWLOG) && self.slowlog_push(client, &redacted(full, args), micros, now) {
            let st = &mut self.stats;
            st.slowlog_count += 1;
            st.slowlog_us_sum += micros;
            st.slowlog_us_max = st.slowlog_us_max.max(micros);
            let stat = self.cmdstats.get_mut(full).unwrap();
            stat.slowlog_count += 1;
            stat.slowlog_us_sum += micros;
            stat.slowlog_us_max = stat.slowlog_us_max.max(micros);
        }
    }

    /// A blocked command ends without running again (timeout, CLIENT UNBLOCK): account for it
    /// now (`updateStatsOnUnblock`).
    fn count_unblocked(&mut self, w: &Waiter, failed: bool) {
        let Ok((cmd, full)) = Self::resolve_as(&w.args, true) else { return };
        self.count_call(&w.client, &full, cmd, &w.args, w.micros, now_ms(), Mode::Unblock);
        if failed {
            self.cmdstats.get_mut(&full).unwrap().failed += 1;
            self.stats.failed_calls += 1;
        }
        if let Some(e) = self.clients.by_id.get(&w.client.id) {
            e.stats.cmds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Log the command if it ran for at least `slowlog-log-slower-than`. Returns whether it did.
    fn slowlog_push(&mut self, client: &Client, args: &[Bytes], micros: u64, now: i64) -> bool {
        let threshold = self.cfg.slowlog_log_slower_than;
        if threshold < 0 || micros < threshold as u64 {
            return false;
        }
        let mut kept: Vec<Bytes> = Vec::new();
        let (max_args, max_len) = (self.cfg.slowlog_max_argc, self.cfg.slowlog_max_string_len);
        for (i, a) in args.iter().enumerate() {
            if i == max_args - 1 && args.len() > max_args {
                kept.push(Bytes::from(format!("... ({} more arguments)", args.len() - max_args + 1)));
                break;
            }
            if a.len() > max_len {
                let mut s = a[..max_len].to_vec();
                s.extend_from_slice(format!("... ({} more bytes)", a.len() - max_len).as_bytes());
                kept.push(Bytes::from(s));
            } else {
                kept.push(a.clone());
            }
        }
        self.slowlog.push_front(SlowEntry {
            id: self.slowlog_next_id,
            at: now / 1000,
            micros,
            args: kept,
            addr: client.addr.clone(),
            argc: args.len(),
            name: client.name.as_ref().map(|n| String::from_utf8_lossy(n).into_owned()).unwrap_or_default(),
        });
        self.slowlog_next_id += 1;
        while self.slowlog.len() as u64 > self.cfg.slowlog_max_len {
            self.slowlog.pop_back();
        }
        true
    }

    // ---- key lifecycle ----

    /// Delete aggregates left empty by the last operations (emitting `del`).
    pub(crate) fn flush_empty(&mut self) {
        while let Some((db, key)) = self.sig.empty.pop() {
            let d = &mut self.dbs[db as usize];
            if d.keys.get(&key).is_some_and(|e| e.value.is_empty_aggregate()) {
                d.remove(&key);
                self.sig.notify(notify::GENERIC, "del", db, &key);
                self.sig.touch_quiet(db, &key);
            }
        }
    }

    /// The clock keys are checked against (`keyIsExpired`): before every expiry while DEBUG
    /// SET-ALLOW-ACCESS-EXPIRED is on.
    pub(crate) fn expiry_clock(&self, now: i64) -> i64 {
        if self.debug.allow_access_expired { i64::MIN } else { now }
    }

    /// If `key` is logically expired, delete it (on a primary) and report true.
    pub(crate) fn expire_if_needed(&mut self, db: usize, key: &[u8], now: i64, mode: Mode) -> bool {
        if self.debug.allow_access_expired {
            return false;
        }
        let Some(e) = self.dbs[db].keys.get(key) else { return false };
        if !e.expired(now) {
            // A hash whose registered field expiry is due: run its active field expiry now (Redis
            // does it from the cron), so the command sees what it would see on Redis. Not while
            // active expiry is off (DEBUG SET-ACTIVE-EXPIRE 0) or paused, like the cron. True if
            // that deleted the whole hash.
            if let Value::Hash(h) = &e.value
                && h.subexpiry_due(now)
                && mode != Mode::Replay
                && !self.replica
                && self.debug.active_expire
                && !self.pause_active(now)
            {
                return commands::hashes::expire_fields(self, db, key, now);
            }
            return false;
        }
        if mode == Mode::Replay {
            // The primary decides when keys expire; it will send the DEL.
            return false;
        }
        // Replicas, and a primary during CLIENT PAUSE, hide the key without deleting it.
        if self.replica || self.pause_active(now) {
            return true;
        }
        self.delete_expired(db, key);
        true
    }

    pub(crate) fn delete_expired(&mut self, db: usize, key: &[u8]) {
        if self.dbs[db].remove(key).is_none() {
            return;
        }
        self.stats.expired_keys += 1;
        self.sig.notify(notify::EXPIRED, "expired", db as u16, key);
        // Not a change of the current command: expiry DELs are propagated on their own, before
        // the command that noticed them.
        self.sig.touch_quiet(db as u16, key);
        self.effects.push(Effect { db: db as u16, args: vec![Bytes::from_static(b"DEL"), Bytes::copy_from_slice(key)] });
    }

    pub(crate) fn process_touched(&mut self) {
        if self.sig.touched.is_empty() {
            return;
        }
        for (db, key) in std::mem::take(&mut self.sig.touched) {
            self.dbs[db as usize].resize(&key);
            let k = (db, key);
            if let Some(ids) = self.watches.keys.get(&k) {
                self.watches.dirty.extend(ids.iter().copied());
            }
            self.clients.invalidate(&k.1, true);
            if self.blocking.by_key.contains_key(&k) && self.blocking.ready_set.insert(k.clone()) {
                self.blocking.ready.push(k);
            }
        }
    }

    pub(crate) fn flush_events(&mut self) {
        if self.sig.events.is_empty() {
            return;
        }
        if self.sig.events.iter().any(|e| e.0 == notify::SEARCH_PROBE) {
            crate::commands::ft::resolve_probes(self);
        }
        let events = std::mem::take(&mut self.sig.events);
        let flags = self.cfg.notify;
        for (_, event, db, key, subkeys) in events {
            if flags & notify::KEYSPACE != 0 {
                let mut ch = format!("__keyspace@{db}__:").into_bytes();
                ch.extend_from_slice(&key);
                self.hub.publish(&ch, event.as_bytes());
            }
            if flags & notify::KEYEVENT != 0 {
                let ch = format!("__keyevent@{db}__:{event}");
                self.hub.publish(ch.as_bytes(), &key);
            }
            if subkeys.is_empty() {
                continue;
            }
            // `<len>:<subkey>[,<len>:<subkey>…]`
            let list = || {
                let mut p = Vec::new();
                for (i, f) in subkeys.iter().enumerate() {
                    if i > 0 {
                        p.push(b',');
                    }
                    p.extend_from_slice(format!("{}:", f.len()).as_bytes());
                    p.extend_from_slice(f);
                }
                p
            };
            // A '|' in the event name would make the separator ambiguous.
            let plain = !event.contains('|');
            if flags & notify::SUBKEYSPACE != 0 && plain {
                let mut ch = format!("__subkeyspace@{db}__:").into_bytes();
                ch.extend_from_slice(&key);
                let mut p = format!("{event}|").into_bytes();
                p.extend(list());
                self.hub.publish(&ch, &p);
            }
            if flags & notify::SUBKEYEVENT != 0 {
                let ch = format!("__subkeyevent@{db}__:{event}");
                let mut p = format!("{}:", key.len()).into_bytes();
                p.extend_from_slice(&key);
                p.push(b'|');
                p.extend(list());
                self.hub.publish(ch.as_bytes(), &p);
            }
            if flags & notify::SUBKEYSPACEITEM != 0 && !key.contains(&b'\n') {
                for f in &subkeys {
                    let mut ch = format!("__subkeyspaceitem@{db}__:").into_bytes();
                    ch.extend_from_slice(&key);
                    ch.push(b'\n');
                    ch.extend_from_slice(f);
                    self.hub.publish(&ch, event.as_bytes());
                }
            }
            if flags & notify::SUBKEYSPACEEVENT != 0 && plain {
                let mut ch = format!("__subkeyspaceevent@{db}__:{event}|").into_bytes();
                ch.extend_from_slice(&key);
                self.hub.publish(&ch, &list());
            }
        }
    }

    /// Touch every watched key of `db` (all dbs when `None`) that currently exists: FLUSHDB,
    /// FLUSHALL, SWAPDB invalidate transactions watching them.
    pub(crate) fn touch_all_watched(&mut self, db: Option<u16>) {
        let mut dirty = Vec::new();
        for ((d, key), ids) in &self.watches.keys {
            if db.is_some_and(|x| x != *d) {
                continue;
            }
            if self.dbs[*d as usize].keys.contains_key(key) {
                dirty.extend(ids.iter().copied());
            }
        }
        self.watches.dirty.extend(dirty);
    }

    // ---- WATCH ----

    pub(crate) fn watch(&mut self, client: &Client, key: &[u8], now: i64) {
        let now = self.expiry_clock(now);
        let db = client.db as u16;
        let list = self.watches.by_client.entry(client.id).or_default();
        if list.iter().any(|(d, k, _)| *d == db && k.as_slice() == key) {
            return;
        }
        let missing = self.dbs[db as usize].keys.get(key).is_none_or(|e| e.expired(now));
        list.push((db, Str::from_slice(key), missing));
        self.watches.keys.entry((db, Str::from_slice(key))).or_default().insert(client.id);
    }

    pub(crate) fn unwatch_all(&mut self, client_id: u64) {
        if let Some(list) = self.watches.by_client.remove(&client_id) {
            for (db, key, _) in list {
                let k = (db, key);
                if let Some(ids) = self.watches.keys.get_mut(&k) {
                    ids.remove(&client_id);
                    if ids.is_empty() {
                        self.watches.keys.remove(&k);
                    }
                }
            }
        }
        self.watches.dirty.remove(&client_id);
    }

    /// Would EXEC abort because a watched key changed (or expired since WATCH)?
    pub(crate) fn watch_failed(&self, client_id: u64, now: i64) -> bool {
        if self.watches.dirty.contains(&client_id) {
            return true;
        }
        let now = self.expiry_clock(now);
        self.watches.by_client.get(&client_id).is_some_and(|list| {
            list.iter().any(|(db, key, missing)| {
                !missing && self.dbs[*db as usize].keys.get(key).is_some_and(|e| e.expired(now))
            })
        })
    }

    // ---- blocking ----

    fn register_waiter(&mut self, client: &Client, args: &[Bytes], b: BlockReq, micros: u64) {
        let db = client.db as u16;
        let keys: Vec<(u16, Str)> = b.keys.into_iter().map(|k| (db, k)).collect();
        for k in &keys {
            self.blocking.by_key.entry(k.clone()).or_default().push_back(client.id);
        }
        self.stats.total_blocking_keys = self.blocking.by_key.len() as u64;
        let mut c = client.clone();
        c.multi = None;
        let args = b.args.unwrap_or_else(|| args.to_vec());
        let w = Waiter {
            client: c,
            args,
            keys,
            timeout_reply: b.timeout_reply,
            deadline: b.deadline,
            unblock_on_nokey: b.unblock_on_nokey,
            wake_at: b.wake_at,
            micros,
            rerun_on_timeout: b.rerun_on_timeout,
        };
        self.blocking.waiters.insert(client.id, w);
    }

    fn unregister_waiter(&mut self, id: u64) -> Option<Waiter> {
        let w = self.blocking.waiters.remove(&id)?;
        for k in &w.keys {
            if let Some(q) = self.blocking.by_key.get_mut(k) {
                q.retain(|x| *x != id);
                if q.is_empty() {
                    self.blocking.by_key.remove(k);
                }
            }
        }
        self.stats.total_blocking_keys = self.blocking.by_key.len() as u64;
        Some(w)
    }

    pub fn blocked_clients(&self) -> usize {
        self.blocking.waiters.len()
    }

    /// When a blocked client times out or must be re-run (absolute Unix ms, 0 = never).
    pub fn block_deadline(&self, client_id: u64) -> Option<i64> {
        self.blocking.waiters.get(&client_id).map(|w| match (w.deadline, w.wake_at) {
            (d, 0) => d,
            (0, wk) => wk,
            (d, wk) => d.min(wk),
        })
    }

    pub fn is_blocked(&self, client_id: u64) -> bool {
        self.blocking.waiters.contains_key(&client_id)
    }

    /// The client's block timed out: returns the reply to send, if it was still blocked. A
    /// client with a wake-up time before its deadline is re-run instead (and may stay blocked:
    /// `None`, with [`Engine::block_deadline`] giving the next time).
    pub fn block_timeout(&mut self, client_id: u64) -> Option<Reply> {
        let w = self.blocking.waiters.get(&client_id)?;
        let now = now_ms();
        if w.wake_at != 0 && (w.deadline == 0 || now < w.deadline) {
            let mut client = w.client.clone();
            let args = w.args.clone();
            match self.dispatch(&mut client, &args, now, Mode::Unblock) {
                Outcome::Blocked => return None,
                Outcome::Reply(r) => {
                    self.unregister_waiter(client_id);
                    self.serve_blocked(now);
                    return Some(r);
                }
            }
        }
        if w.rerun_on_timeout {
            return self.rerun_timed_out(client_id, now);
        }
        let w = self.unregister_waiter(client_id)?;
        self.count_unblocked(&w, false);
        self.serve_blocked(now);
        Some(w.timeout_reply)
    }

    /// A module's timeout callback: the command runs once more to answer with what it has.
    fn rerun_timed_out(&mut self, client_id: u64, now: i64) -> Option<Reply> {
        let w = self.blocking.waiters.get(&client_id)?;
        let mut client = w.client.clone();
        let args = w.args.clone();
        self.blocking.timing_out = true;
        let out = self.dispatch(&mut client, &args, now, Mode::Unblock);
        self.blocking.timing_out = false;
        self.unregister_waiter(client_id);
        self.serve_blocked(now);
        match out {
            Outcome::Reply(r) => Some(r),
            Outcome::Blocked => Some(Reply::NilArray),
        }
    }

    /// `CLIENT UNBLOCK id [TIMEOUT|ERROR]`.
    /// Redis' disconnectAllBlockedClients, when the instance becomes a replica: every blocked
    /// command ends with an error and its connection closes after the reply.
    pub fn unblock_all_for_role_change(&mut self) {
        let ids: Vec<u64> = self.blocking.waiters.keys().copied().collect();
        for id in ids {
            let Some(w) = self.unregister_waiter(id) else { continue };
            self.count_unblocked(&w, true);
            let r = Reply::error("UNBLOCKED force unblock from blocking operation, instance state changed (master -> replica?)");
            self.count_error(&r);
            if let Some(e) = self.clients.by_id.get(&id) {
                e.stats.killed.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(sink) = &w.client.sink {
                sink.unblock(r.to_bytes(w.client.resp));
            }
        }
    }

    pub fn unblock(&mut self, client_id: u64, error: bool) -> Option<Reply> {
        if !error && self.blocking.waiters.get(&client_id).is_some_and(|w| w.rerun_on_timeout) {
            return self.rerun_timed_out(client_id, now_ms());
        }
        let w = self.unregister_waiter(client_id)?;
        self.count_unblocked(&w, error);
        if !error {
            return Some(w.timeout_reply);
        }
        let r = Reply::error("UNBLOCKED client unblocked via CLIENT UNBLOCK");
        self.count_error(&r);
        Some(r)
    }

    /// Serve clients blocked on keys that became ready.
    pub fn serve_blocked(&mut self, now: i64) {
        let mut rounds = 0;
        while !self.blocking.ready.is_empty() && rounds < 1000 {
            rounds += 1;
            let ready = std::mem::take(&mut self.blocking.ready);
            self.blocking.ready_set.clear();
            for k in ready {
                let Some(q) = self.blocking.by_key.get(&k) else { continue };
                let ids: Vec<u64> = q.iter().copied().collect();
                for id in ids {
                    let Some(w) = self.blocking.waiters.get(&id) else { continue };
                    if !w.unblock_on_nokey && !self.dbs[k.0 as usize].keys.contains_key(&k.1) {
                        continue;
                    }
                    let mut client = w.client.clone();
                    let args = w.args.clone();
                    let (current, defer) = (self.clients.current, self.clients.defer_current);
                    self.clients.current = id;
                    self.clients.defer_current = false;
                    match self.dispatch(&mut client, &args, now, Mode::Unblock) {
                        Outcome::Blocked => {}
                        Outcome::Reply(r) => {
                            self.unregister_waiter(id);
                            if let Some(sink) = &client.sink {
                                sink.unblock(r.to_bytes(client.resp));
                            }
                            self.clients.flush_pending();
                        }
                    }
                    self.clients.current = current;
                    self.clients.defer_current = defer;
                }
            }
        }
    }

    /// Forget everything about a disconnected client.
    pub fn remove_client(&mut self, client_id: u64) {
        self.unregister_waiter(client_id);
        self.unwatch_all(client_id);
        self.clients.unregister(client_id);
    }

    // ---- LRU / LFU ----

    pub fn lru_clock(now: i64) -> u32 {
        ((now / 1000) as u32) & LRU_CLOCK_MAX
    }

    pub(crate) fn new_lru(&self, now: i64) -> u32 {
        if self.cfg.maxmemory_policy.is_lfu() {
            (lfu_minutes(now) << 8) | LFU_INIT_VAL
        } else {
            Self::lru_clock(now)
        }
    }

    pub(crate) fn touch_entry(cfg: &Config, rng: &mut Rng, e: &mut Entry, now: i64) {
        if cfg.maxmemory_policy.is_lrm() {
            return;
        }
        if cfg.maxmemory_policy.is_lfu() {
            let counter = lfu_decr(cfg, e.lru, now);
            let counter = lfu_incr(cfg, rng, counter);
            e.lru = (lfu_minutes(now) << 8) | counter;
        } else {
            e.lru = Self::lru_clock(now);
        }
    }

    /// `objectSetLRUOrLFU`: the LRU/LFU field of a key restored with a frequency (LFU policies)
    /// or an idle time in seconds (other policies), as RESTORE and RDB loading do; a fresh one
    /// when the matching hint is missing.
    pub fn restored_lru(cfg: &Config, freq: Option<u8>, idle: Option<u64>, now: i64) -> u32 {
        let lfu = cfg.maxmemory_policy.is_lfu();
        if lfu && let Some(f) = freq {
            return (lfu_minutes(now) << 8) | f as u32;
        }
        if !lfu && let Some(idle) = idle.and_then(|i| i64::try_from(i).ok()) {
            // `lru_idle * 1000 / LRU_CLOCK_RESOLUTION`, wrapping like C, then unwrap the clock.
            let idle = idle.wrapping_mul(1000) / 1000;
            let mut abs = (Self::lru_clock(now) as i64).wrapping_sub(idle);
            if abs < 0 {
                abs = abs.wrapping_add(LRU_CLOCK_MAX as i64);
            }
            return abs as u32 & LRU_CLOCK_MAX;
        }
        if lfu { (lfu_minutes(now) << 8) | LFU_INIT_VAL } else { Self::lru_clock(now) }
    }

    /// Seconds since last access (`OBJECT IDLETIME`).
    pub fn idle_secs(e: &Entry, now: i64) -> u64 {
        let clock = Self::lru_clock(now);
        let lru = e.lru & LRU_CLOCK_MAX;
        (if clock >= lru { clock - lru } else { LRU_CLOCK_MAX - lru + clock }) as u64
    }

    /// `OBJECT FREQ`.
    pub fn lfu_counter(&self, e: &Entry, now: i64) -> u32 {
        lfu_decr(&self.cfg, e.lru, now)
    }

    // ---- maxmemory ----

    /// Evict keys until under `maxmemory`. Returns false if still over (or eviction disabled).
    pub fn evict_to_fit(&mut self, now: i64) -> bool {
        if self.exec_depth == 0 {
            self.exec_time = now;
        }
        let max = self.cfg.maxmemory as usize;
        if max == 0 || self.used_memory() <= max {
            return true;
        }
        if self.cfg.maxmemory_policy == Policy::NoEviction || self.pause_active(now) {
            return false;
        }
        let start = Instant::now();
        let mut freed_any = false;
        while self.used_memory() > max {
            let Some((db, key)) = self.pick_victim(now) else { break };
            self.dbs[db as usize].remove(&key);
            self.stats.evicted_keys += 1;
            self.sig.notify(notify::EVICTED, "evicted", db, &key);
            self.sig.touch_quiet(db, &key);
            self.effects.push(Effect { db, args: vec![Bytes::from_static(b"DEL"), Bytes::copy_from_slice(&key)] });
            freed_any = true;
            if start.elapsed().as_micros() as u64 > 500 * self.cfg.maxmemory_eviction_tenacity.max(1) as u64 {
                break;
            }
        }
        if freed_any {
            self.process_touched();
            self.flush_events();
        }
        self.stats.total_eviction_exceeded_time_ms += start.elapsed().as_millis() as u64;
        self.used_memory() <= max
    }

    fn pick_victim(&mut self, now: i64) -> Option<(u16, Str)> {
        let policy = self.cfg.maxmemory_policy;
        let volatile = policy.volatile_only();
        if matches!(policy, Policy::AllKeysRandom | Policy::VolatileRandom) {
            let n = self.dbs.len();
            let off = self.rng.below(n);
            for i in 0..n {
                let db = (off + i) % n;
                let d = &self.dbs[db];
                let k = if volatile { d.volatile.random(&mut self.rng).map(|(k, _)| k.clone()) } else { d.keys.random(&mut self.rng).map(|(k, _)| k.clone()) };
                if let Some(k) = k {
                    return Some((db as u16, k));
                }
            }
            return None;
        }
        // Approximated LRU/LFU/TTL with a pool of the best candidates seen (like Redis).
        let samples = self.cfg.maxmemory_samples as usize;
        for db in 0..self.dbs.len() {
            let d = &self.dbs[db];
            let mut cands: Vec<Str> = Vec::with_capacity(samples);
            if volatile {
                d.volatile.sample(&mut self.rng, samples, |k, _| cands.push(k.clone()));
            } else {
                d.keys.sample(&mut self.rng, samples, |k, _| cands.push(k.clone()));
            }
            for k in cands {
                let Some(e) = d.keys.get(&k) else { continue };
                let score = match policy {
                    Policy::VolatileTtl => u64::MAX - e.expire.max(0) as u64,
                    p if p.is_lfu() => 255 - lfu_decr(&self.cfg, e.lru, now) as u64,
                    _ => Self::idle_secs(e, now) * 1000 + 1,
                };
                if self.evict_pool.iter().any(|(_, pdb, pk)| *pdb == db as u16 && *pk == k) {
                    continue;
                }
                self.evict_pool.push((score, db as u16, k));
            }
        }
        self.evict_pool.sort_by_key(|e| std::cmp::Reverse(e.0));
        self.evict_pool.truncate(EVPOOL_SIZE);
        while !self.evict_pool.is_empty() {
            let (_, db, k) = self.evict_pool.remove(0);
            if self.dbs[db as usize].keys.contains_key(&k) {
                return Some((db, k));
            }
        }
        None
    }

    // ---- active expiry ----

    /// One active-expiry pass (call ~`hz` times a second). Returns keys expired.
    pub fn active_expire(&mut self, budget_us: u64) -> usize {
        if self.replica || !self.debug.active_expire {
            return 0;
        }
        let now = now_ms();
        if self.exec_depth == 0 {
            self.exec_time = now;
        }
        if let Some(h) = self.hotkeys.as_mut() {
            h.expire(now);
        }
        if self.pause_active(now) {
            return 0;
        }
        let start = Instant::now();
        let per_loop = 20 + 5 * (self.cfg.active_expire_effort as usize - 1).min(9);
        let mut total = 0;
        self.sig.notify = self.cfg.notify;
        self.sig.track = true;
        for db in 0..self.dbs.len() {
            loop {
                if self.dbs[db].volatile.is_empty() {
                    break;
                }
                let mut sampled = 0;
                let mut expired: Vec<Str> = Vec::new();
                let mut ttl_sum = 0i64;
                let mut ttl_n = 0i64;
                let mut cursor = self.dbs[db].expire_cursor;
                while sampled < per_loop {
                    let d = &self.dbs[db];
                    cursor = d.volatile.scan(cursor, |k, _| {
                        sampled += 1;
                        if let Some(e) = d.keys.get(k) {
                            if e.expired(now) {
                                expired.push(k.clone());
                            } else if e.expire != 0 {
                                ttl_sum += e.expire - now;
                                ttl_n += 1;
                            }
                        }
                    });
                    if cursor == 0 {
                        break;
                    }
                }
                self.dbs[db].expire_cursor = cursor;
                if ttl_n > 0 {
                    let avg = ttl_sum / ttl_n;
                    let d = &mut self.dbs[db];
                    d.avg_ttl = if d.avg_ttl == 0 { avg } else { (d.avg_ttl / 50) * 49 + avg / 50 };
                }
                let n = expired.len();
                for k in expired {
                    self.delete_expired(db, &k);
                }
                self.stats.expired_keys_active += n as u64;
                total += n;
                if start.elapsed().as_micros() as u64 > budget_us {
                    self.stats.expired_time_cap_reached += 1;
                    break;
                }
                // Keep going while more than 10% of the sample was expired.
                if sampled == 0 || n * 10 <= sampled || cursor == 0 {
                    break;
                }
            }
            // Hash fields with a TTL (Redis' activeSubexpiresCycle).
            let fields = commands::hashes::active_expire_fields(self, db, now, start, budget_us);
            self.stats.expired_subkeys_active += fields as u64;
            total += fields;
        }
        self.stats.expire_cycle_us += start.elapsed().as_micros() as u64;
        if total > 0 {
            self.process_touched();
            self.flush_events();
            self.serve_blocked(now);
        }
        total
    }

    /// Incremental rehashing of the main tables (call from the cron).
    pub fn rehash(&mut self, steps: usize) {
        for d in &mut self.dbs {
            d.keys.rehash(steps);
            d.volatile.rehash(steps);
        }
    }

    /// A key was loaded (RDB, full resync): RedisTimeSeries' `loaded` event indexes a series
    /// (`IndexMetricFromName`, a counted lookup once it listens).
    pub fn ts_loaded(&mut self, db: usize, key: &[u8]) {
        let Some(e) = self.dbs[db].keys.get(key) else { return };
        let labels = match &e.value {
            crate::value::Value::Ts(s) => {
                self.sig.ts_hook = true;
                Some(s.labels.clone())
            }
            _ => None,
        };
        if !self.sig.ts_hook {
            return;
        }
        self.stats.keyspace_hits += 1;
        if let Some(labels) = labels {
            self.ts.index.remove(key);
            self.ts.index.add(key, &labels);
        }
    }

    pub fn flush_all(&mut self) {
        self.ts.index.clear();
        self.touch_all_watched(None);
        self.clients.flushed();
        for d in &mut self.dbs {
            d.clear();
        }
        self.evict_pool.clear();
    }
}

pub const LRU_CLOCK_MAX: u32 = (1 << 24) - 1;
const LFU_INIT_VAL: u32 = 5;
const EVPOOL_SIZE: usize = 16;

/// Wrap the effects from `start` on (one execution unit) in MULTI/EXEC if they are more than one
/// command; markers a command placed itself inside the unit are replaced by the unit's.
fn wrap_unit(effects: &mut Vec<Effect>, start: usize) {
    let marker = |e: &Effect| e.args.len() == 1 && (commands::eq(&e.args[0], "multi") || commands::eq(&e.args[0], "exec"));
    let unit = &effects[start..];
    let ops = unit.iter().filter(|e| !marker(e)).count();
    if ops < 2 {
        return;
    }
    if unit.len() - ops == 2 && marker(&unit[0]) && marker(&unit[unit.len() - 1]) {
        return;
    }
    let mut ops: Vec<Effect> = effects.drain(start..).filter(|e| !marker(e)).collect();
    let db = ops[0].db;
    effects.push(Effect { db, args: vec![Bytes::from_static(b"MULTI")] });
    effects.append(&mut ops);
    effects.push(Effect { db, args: vec![Bytes::from_static(b"EXEC")] });
}

fn lfu_minutes(now: i64) -> u32 {
    ((now / 60_000) as u32) & 65535
}

fn lfu_decr(cfg: &Config, lru: u32, now: i64) -> u32 {
    let ldt = lru >> 8;
    let counter = lru & 255;
    let m = lfu_minutes(now);
    let elapsed = if m >= ldt { m - ldt } else { 65535 - ldt + m };
    let periods = elapsed.checked_div(cfg.lfu_decay_time).unwrap_or(0);
    if periods > 0 { counter.saturating_sub(periods) } else { counter }
}

fn lfu_incr(cfg: &Config, rng: &mut Rng, counter: u32) -> u32 {
    if counter >= 255 {
        return 255;
    }
    let base = counter.saturating_sub(LFU_INIT_VAL) as f64;
    let p = 1.0 / (base * cfg.lfu_log_factor as f64 + 1.0);
    if rng.unit() < p { counter + 1 } else { counter }
}

impl From<Error> for Outcome {
    fn from(e: Error) -> Self {
        Outcome::Reply(e.into())
    }
}
