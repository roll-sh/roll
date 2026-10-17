//! Lua scripting (EVAL, EVALSHA, SCRIPT) and functions (FUNCTION, FCALL), on the Lua 5.1 Redis
//! embeds (see `redis-lua`).
//!
//! Scripts run atomically inside the engine. Commands they call go through the normal dispatch as
//! a separate script client (its own selected db and RESP version, the caller's user), and their
//! effects are what gets replicated: a script's writes are wrapped in MULTI/EXEC, never the
//! script itself. Functions are data: FUNCTION LOAD / DELETE / FLUSH replicate as themselves.
//! The script cache is not (EVALSHA after a restart answers NOSCRIPT, as in Redis).

mod vm;

use super::*;
use crate::{cmd, container};
use crate::client::{Client, ReplyMode};
use crate::engine::{Engine, Mode};
use redis_lua::*;
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_int;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use vm::{FnDef, Flavor, Vm};

pub static COMMANDS: &[Command] = &[
    Command { since: "2.6.0", ..cmd!("eval", -3, NOSCRIPT | SKIP_MONITOR | SCRIPT_RUNNER | MAY_REPLICATE | NO_MANDATORY_KEYS | STALE, keys = script_keys, eval, Scripting) },
    Command { since: "2.6.0", ..cmd!("evalsha", -3, NOSCRIPT | SKIP_MONITOR | SCRIPT_RUNNER | MAY_REPLICATE | NO_MANDATORY_KEYS | STALE, keys = script_keys, evalsha, Scripting) },
    Command { since: "7.0.0", ..cmd!("eval_ro", -3, NOSCRIPT | SKIP_MONITOR | SCRIPT_RUNNER | NO_MANDATORY_KEYS | STALE | READONLY, keys = script_keys, eval_ro, Scripting) },
    Command { since: "7.0.0", ..cmd!("evalsha_ro", -3, NOSCRIPT | SKIP_MONITOR | SCRIPT_RUNNER | NO_MANDATORY_KEYS | STALE | READONLY, keys = script_keys, evalsha_ro, Scripting) },
    Command { since: "7.0.0", ..cmd!("fcall", -3, NOSCRIPT | SKIP_MONITOR | SCRIPT_RUNNER | MAY_REPLICATE | NO_MANDATORY_KEYS | STALE, keys = script_keys, fcall, Scripting) },
    Command { since: "7.0.0", ..cmd!("fcall_ro", -3, NOSCRIPT | SKIP_MONITOR | SCRIPT_RUNNER | NO_MANDATORY_KEYS | STALE | READONLY, keys = script_keys, fcall_ro, Scripting) },
    Command { since: "2.6.0", ..container!("script", 0, Scripting, SCRIPT) },
    Command { since: "7.0.0", ..container!("function", 0, Scripting, FUNCTION) },
];

static SCRIPT: &[Command] = &[
    Command { since: "3.2.0", ..cmd!("debug", 3, NOSCRIPT, 0, 0, 0, script_debug, Scripting) },
    Command { since: "2.6.0", ..cmd!("exists", -3, NOSCRIPT, 0, 0, 0, script_exists, Scripting) },
    Command { since: "2.6.0", ..cmd!("flush", -2, NOSCRIPT, 0, 0, 0, script_flush, Scripting) },
    Command { since: "5.0.0", ..cmd!("help", 2, LOADING | STALE, 0, 0, 0, script_help, Scripting) },
    Command { since: "2.6.0", ..cmd!("kill", 2, NOSCRIPT | ALLOW_BUSY, 0, 0, 0, script_kill, Scripting) },
    Command { since: "2.6.0", ..cmd!("load", 3, NOSCRIPT | STALE, 0, 0, 0, script_load, Scripting) },
];

static FUNCTION: &[Command] = &[
    Command { since: "7.0.0", ..cmd!("delete", 3, NOSCRIPT | WRITE, 0, 0, 0, function_delete, Scripting) },
    Command { since: "7.0.0", ..cmd!("dump", 2, NOSCRIPT, 0, 0, 0, function_dump, Scripting) },
    Command { since: "7.0.0", ..cmd!("flush", -2, NOSCRIPT | WRITE, 0, 0, 0, function_flush, Scripting) },
    Command { since: "7.0.0", ..cmd!("help", 2, LOADING | STALE, 0, 0, 0, function_help, Scripting) },
    Command { since: "7.0.0", ..cmd!("kill", 2, NOSCRIPT | ALLOW_BUSY, 0, 0, 0, function_kill, Scripting) },
    Command { since: "7.0.0", ..cmd!("list", -2, NOSCRIPT, 0, 0, 0, function_list, Scripting) },
    Command { since: "7.0.0", ..cmd!("load", -3, NOSCRIPT | WRITE | DENYOOM, 0, 0, 0, function_load, Scripting) },
    Command { since: "7.0.0", ..cmd!("restore", -3, NOSCRIPT | WRITE | DENYOOM, 0, 0, 0, function_restore, Scripting) },
    Command { since: "7.0.0", ..cmd!("stats", 2, NOSCRIPT | ALLOW_BUSY, 0, 0, 0, function_stats, Scripting) },
];

/// `numkeys key [key ...] arg [arg ...]` after the script / function name.
fn script_keys(args: &[Bytes]) -> Vec<usize> {
    match args.get(2).and_then(|n| crate::util::parse_i64(n)) {
        Some(n) if n >= 0 && (n as usize) <= args.len().saturating_sub(3) => (3..3 + n as usize).collect(),
        _ => Vec::new(),
    }
}

// ---- flags ----

pub const NO_WRITES: u64 = 1 << 0;
pub const ALLOW_OOM: u64 = 1 << 1;
pub const ALLOW_STALE: u64 = 1 << 2;
pub const NO_CLUSTER: u64 = 1 << 3;
/// An EVAL script without a shebang: Redis' backwards-compatible behaviour.
pub const COMPAT: u64 = 1 << 4;
pub const ALLOW_CROSS_SLOT: u64 = 1 << 5;

const FLAG_NAMES: [(u64, &str); 5] = [
    (NO_WRITES, "no-writes"),
    (ALLOW_OOM, "allow-oom"),
    (ALLOW_STALE, "allow-stale"),
    (NO_CLUSTER, "no-cluster"),
    (ALLOW_CROSS_SLOT, "allow-cross-slot-keys"),
];

pub(crate) fn script_flag(name: &[u8]) -> Option<u64> {
    FLAG_NAMES.iter().find(|(_, n)| n.as_bytes() == name).map(|(f, _)| *f)
}

/// Letters, digits and underscores, at least one.
pub(crate) fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn sha1_hex(b: &[u8]) -> String {
    use sha1::Digest;
    let d = sha1::Sha1::digest(b);
    let mut s = String::with_capacity(40);
    for byte in d {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// A `#!lua flags=...` first line: the flags and the length of the line (kept out of the code but
/// its newline stays, so line numbers match). No shebang: [`COMPAT`].
fn shebang_flags(body: &[u8]) -> Result<(u64, usize), String> {
    if !body.starts_with(b"#!") {
        return Ok((COMPAT, 0));
    }
    let end = body.iter().position(|&c| c == b'\n').ok_or("Invalid script shebang")?;
    let parts = match redis_proto::split_args(&body[..end]) {
        Some(p) if !p.is_empty() => p,
        _ => return Err("Invalid engine in script shebang".into()),
    };
    if parts[0] != b"#!lua" {
        return Err(format!("Unexpected engine in script shebang: {}", String::from_utf8_lossy(&parts[0])));
    }
    let mut flags = 0;
    for p in &parts[1..] {
        let Some(list) = p.strip_prefix(b"flags=") else {
            return Err(format!("Unknown lua shebang option: {}", String::from_utf8_lossy(p)));
        };
        if list.is_empty() {
            continue;
        }
        for f in list.split(|&c| c == b',') {
            flags |= script_flag(f).ok_or_else(|| format!("Unexpected flag in script shebang: {}", String::from_utf8_lossy(f)))?;
        }
    }
    Ok((flags, end))
}

// ---- state ----

/// What other connections can see of a running script: the server answers BUSY while `busy`,
/// and SCRIPT KILL / FUNCTION KILL set `kill`.
#[derive(Default)]
pub struct ScriptCtl {
    pub busy: AtomicBool,
    pub kill: AtomicBool,
    /// The running script is an EVAL (not a function).
    pub eval: AtomicBool,
    /// It already wrote: it can't be killed any more.
    pub dirty: AtomicBool,
    /// Sent by the primary or replayed from the log: it must finish.
    pub obey: AtomicBool,
    /// Library and function counts, for FUNCTION STATS while busy.
    pub libraries: AtomicUsize,
    pub functions: AtomicUsize,
    /// Name and command of the running script, and when it started (FUNCTION STATS).
    pub info: parking_lot::Mutex<Option<(String, Vec<Bytes>, Instant)>>,
}

struct Script {
    body: Bytes,
    flags: u64,
    /// Position in the eviction order; `None` for SCRIPT LOAD scripts (never evicted).
    stamp: Option<u64>,
}

pub struct Library {
    pub name: String,
    /// The whole code, shebang included.
    pub code: Bytes,
    functions: Vec<FnDef>,
}

/// Only EVAL scripts count towards this; the oldest are dropped first.
const MAX_EVAL_SCRIPTS: usize = 500;
const LOAD_TIMEOUT_MS: u64 = 500;

/// Per-shard scripting state: the EVAL VM and script cache, the functions VM and libraries.
#[derive(Default)]
pub struct Scripting {
    eval_vm: Option<Vm>,
    scripts: HashMap<String, Script>,
    lru: BTreeMap<u64, String>,
    next_stamp: u64,
    scripts_mem: usize,
    fn_vm: Option<Vm>,
    libs: Vec<Library>,
    /// Lowercase function name -> (library, function) index.
    fn_index: HashMap<String, (usize, usize)>,
    pub(crate) running: Option<Running>,
    pub ctl: Arc<ScriptCtl>,
}

/// The running script, as command dispatch needs to see it.
pub(crate) struct Running {
    pub allow_oom: bool,
    pub write_dirty: bool,
    /// `redis.set_repl(redis.REPL_NONE)` turns this off.
    pub propagate: bool,
}

impl Scripting {
    /// Bytes used by the EVAL VM (`used_memory_lua` / `used_memory_vm_eval`).
    pub fn memory(&self) -> usize {
        self.eval_vm.as_ref().map_or(0, Vm::memory)
    }

    /// Bytes used by the functions VM (`used_memory_vm_functions`).
    pub fn functions_memory(&self) -> usize {
        self.fn_vm.as_ref().map_or(0, Vm::memory)
    }

    /// Script bodies and their bookkeeping (`used_memory_scripts_eval`).
    pub fn scripts_memory(&self) -> usize {
        self.scripts_mem
    }

    /// Library code and bookkeeping (`used_memory_functions`).
    pub fn libraries_memory(&self) -> usize {
        self.libs.iter().map(|l| l.code.len() + l.name.len() + l.functions.iter().map(|f| f.name.len() + 64).sum::<usize>()).sum()
    }

    pub fn cached_scripts(&self) -> usize {
        self.scripts.len()
    }

    /// The flags EVAL / EVALSHA / FCALL with these arguments would run with (`COMPAT` for a
    /// script without a shebang, `None` when it can't be found).
    pub fn flags_for(&self, args: &[Bytes]) -> Option<u64> {
        let name = String::from_utf8_lossy(&args[0]).to_ascii_lowercase();
        if name.starts_with("fcall") {
            let &(li, fi) = self.fn_index.get(&String::from_utf8_lossy(args.get(1)?).to_ascii_lowercase())?;
            return Some(self.libs[li].functions[fi].flags);
        }
        let by_sha = name.starts_with("evalsha");
        let body = args.get(1)?;
        let sha = if by_sha { String::from_utf8_lossy(body).to_ascii_lowercase() } else { sha1_hex(body) };
        match self.scripts.get(&sha) {
            Some(s) => Some(s.flags),
            None if by_sha => None,
            None => Some(shebang_flags(body).map_or(COMPAT, |(f, _)| f)),
        }
    }

    pub fn libraries(&self) -> &[Library] {
        &self.libs
    }

    pub fn functions_count(&self) -> usize {
        self.fn_index.len()
    }

    /// The commands that recreate every library (FUNCTION LOAD each), for snapshots.
    pub fn library_codes(&self) -> Vec<Bytes> {
        self.libs.iter().map(|l| l.code.clone()).collect()
    }

    fn eval_vm(&mut self) -> &mut Vm {
        self.eval_vm.get_or_insert_with(|| Vm::new(Flavor::Eval))
    }

    fn fn_vm(&mut self) -> &mut Vm {
        self.fn_vm.get_or_insert_with(|| Vm::new(Flavor::Functions))
    }

    /// SCRIPT FLUSH: a fresh VM and an empty cache.
    pub fn flush_scripts(&mut self) {
        self.eval_vm = None;
        self.scripts.clear();
        self.lru.clear();
        self.scripts_mem = 0;
    }

    /// FUNCTION FLUSH: a fresh VM and no libraries.
    pub fn flush_functions(&mut self) {
        self.fn_vm = None;
        self.libs.clear();
        self.reindex();
    }

    fn reindex(&mut self) {
        self.fn_index.clear();
        for (li, lib) in self.libs.iter().enumerate() {
            for (fi, f) in lib.functions.iter().enumerate() {
                self.fn_index.insert(f.name.to_ascii_lowercase(), (li, fi));
            }
        }
        self.ctl.libraries.store(self.libs.len(), Ordering::SeqCst);
        self.ctl.functions.store(self.fn_index.len(), Ordering::SeqCst);
    }

    /// Compile a script into the cache (EVAL with `evictable`, SCRIPT LOAD without). Returns its
    /// SHA1.
    fn create_script(&mut self, body: &Bytes, evictable: bool, evicted: &mut u64) -> Result<String, Error> {
        let sha = sha1_hex(body);
        if self.scripts.contains_key(&sha) {
            return Ok(sha);
        }
        let (flags, skip) = shebang_flags(body).map_err(|e| Error::msg(format!("ERR {e}")))?;
        let l = self.eval_vm().l;
        let name = std::ffi::CString::new(format!("f_{sha}")).unwrap();
        unsafe {
            let code = &body[skip..];
            if luaL_loadbuffer(l, code.as_ptr().cast(), code.len(), c"@user_script".as_ptr()) != 0 {
                let msg = vm::to_bytes(l, -1).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                lua_pop(l, 1);
                self.eval_vm().gc_tick();
                return Err(Error::msg(format!("ERR Error compiling script (new function): {}", msg.replace(['\r', '\n'], " "))));
            }
            lua_setfield(l, LUA_REGISTRYINDEX, name.as_ptr());
        }
        let stamp = if evictable {
            while self.lru.len() >= MAX_EVAL_SCRIPTS {
                let (_, oldest) = self.lru.pop_first().expect("non-empty");
                self.delete_script(&oldest);
                *evicted += 1;
            }
            let s = self.next_stamp;
            self.next_stamp += 1;
            self.lru.insert(s, sha.clone());
            Some(s)
        } else {
            None
        };
        self.scripts_mem += sha.len() + body.len();
        self.scripts.insert(sha.clone(), Script { body: body.clone(), flags, stamp });
        self.eval_vm().gc_tick();
        Ok(sha)
    }

    fn delete_script(&mut self, sha: &str) {
        let Some(s) = self.scripts.remove(sha) else { return };
        self.scripts_mem = self.scripts_mem.saturating_sub(sha.len() + s.body.len());
        if let Some(vm) = &self.eval_vm {
            let name = std::ffi::CString::new(format!("f_{sha}")).unwrap();
            unsafe {
                lua_pushnil(vm.l);
                lua_setfield(vm.l, LUA_REGISTRYINDEX, name.as_ptr());
            }
        }
    }

    /// The script was used: it becomes the last to be evicted.
    fn touch_script(&mut self, sha: &str) {
        let Some(s) = self.scripts.get_mut(sha) else { return };
        if let Some(old) = s.stamp {
            self.lru.remove(&old);
            let stamp = self.next_stamp;
            self.next_stamp += 1;
            s.stamp = Some(stamp);
            self.lru.insert(stamp, sha.to_string());
        }
    }

    /// FUNCTION LOAD. Returns the library name; errors are the message without its code.
    pub fn load_library(&mut self, code: &Bytes, replace: bool, timeout_ms: u64) -> Result<String, String> {
        let (engine, name, body) = library_metadata(code)?;
        if !valid_name(name.as_bytes()) {
            return Err("Library names can only contain letters, numbers, or underscores(_) and must be at least one character long".into());
        }
        if !engine.eq_ignore_ascii_case("lua") {
            return Err(format!("Engine '{engine}' not found"));
        }
        let old = self.libs.iter().position(|l| l.name == name);
        if old.is_some() && !replace {
            return Err(format!("Library '{name}' already exists"));
        }
        let vm = self.fn_vm();
        let l = vm.l;
        let functions = unsafe { run_library(vm, body, timeout_ms)? };
        let unref = |fs: &[FnDef]| {
            for f in fs {
                unsafe { luaL_unref(l, LUA_REGISTRYINDEX, f.lua_ref) };
            }
        };
        if functions.is_empty() {
            return Err("No functions registered".into());
        }
        for f in &functions {
            let taken = self.fn_index.get(&f.name.to_ascii_lowercase()).is_some_and(|(li, _)| Some(*li) != old);
            if taken {
                unref(&functions);
                return Err(format!("Function {} already exists", f.name));
            }
        }
        let lib = Library { name: name.clone(), code: code.clone(), functions };
        match old {
            Some(i) => {
                let prev = std::mem::replace(&mut self.libs[i], lib);
                unref(&prev.functions);
            }
            None => self.libs.push(lib),
        }
        self.reindex();
        Ok(name)
    }

    /// FUNCTION RESTORE (and snapshot load): add these libraries, all or none. Errors are the
    /// message without its code.
    /// The EVAL scripts, by SHA1 (DEBUG SCRIPT LIST).
    pub fn eval_scripts(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.scripts.iter().map(|(sha, s)| (sha.as_str(), &s.body[..]))
    }

    /// One EVAL script's body.
    pub fn eval_script(&self, sha: &str) -> Option<&[u8]> {
        self.scripts.get(sha).map(|s| &s.body[..])
    }

    pub fn restore_libraries(&mut self, codes: &[Vec<u8>], policy: RestorePolicy) -> Result<(), String> {
        // Compiled apart first, so a bad payload changes nothing.
        let mut staged = Scripting::default();
        for code in codes {
            staged.load_library(&Bytes::copy_from_slice(code), false, 0)?;
        }
        if policy == RestorePolicy::Flush {
            self.flush_functions();
        } else {
            let replaced = |name: &str| staged.libs.iter().any(|l| l.name == name);
            if policy == RestorePolicy::Append
                && let Some(lib) = staged.libs.iter().find(|l| self.libs.iter().any(|o| o.name == l.name))
            {
                return Err(format!("Library {} already exists", lib.name));
            }
            for f in staged.libs.iter().flat_map(|l| &l.functions) {
                if let Some(&(li, _)) = self.fn_index.get(&f.name.to_ascii_lowercase())
                    && !replaced(&self.libs[li].name)
                {
                    return Err(format!("Function {} already exists", f.name));
                }
            }
            for lib in &staged.libs {
                self.delete_library(lib.name.as_bytes());
            }
        }
        for lib in &staged.libs {
            self.load_library(&lib.code, false, 0)?;
        }
        Ok(())
    }

    pub fn delete_library(&mut self, name: &[u8]) -> bool {
        let Some(i) = self.libs.iter().position(|l| l.name.as_bytes() == name) else { return false };
        let lib = self.libs.remove(i);
        if let Some(vm) = &self.fn_vm {
            for f in &lib.functions {
                unsafe { luaL_unref(vm.l, LUA_REGISTRYINDEX, f.lua_ref) };
            }
        }
        self.reindex();
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RestorePolicy {
    Append,
    Replace,
    Flush,
}

/// `#!<engine> name=<library>` and the code after it (from its newline on).
fn library_metadata(code: &[u8]) -> Result<(String, String, &[u8]), String> {
    if !code.starts_with(b"#!") {
        return Err("Missing library metadata".into());
    }
    let end = code.iter().position(|&c| c == b'\n').ok_or("Invalid library metadata")?;
    let parts = match redis_proto::split_args(&code[..end]) {
        Some(p) if !p.is_empty() => p,
        _ => return Err("Invalid library metadata".into()),
    };
    let engine = String::from_utf8_lossy(&parts[0][2.min(parts[0].len())..]).into_owned();
    let mut name = None;
    for p in &parts[1..] {
        if p.len() >= 5 && p[..5].eq_ignore_ascii_case(b"name=") {
            if name.is_some() {
                return Err("Invalid metadata value, name argument was given multiple times".into());
            }
            name = Some(String::from_utf8_lossy(&p[5..]).into_owned());
            continue;
        }
        return Err(format!("Invalid metadata value given: {}", String::from_utf8_lossy(p)));
    }
    let name = name.ok_or("Library name was not given")?;
    Ok((engine, name, &code[end..]))
}

/// The FUNCTION LOAD in progress, seen by redis.register_function.
pub(crate) struct Load {
    functions: Vec<FnDef>,
    start: Instant,
    timeout_ms: u64,
}

impl Load {
    pub fn timed_out(&self) -> bool {
        self.timeout_ms > 0 && self.start.elapsed().as_millis() as u64 > self.timeout_ms
    }
}

/// Run library code so it registers its functions.
unsafe fn run_library(vm: &mut Vm, body: &[u8], timeout_ms: u64) -> Result<Vec<FnDef>, String> {
    let l = vm.l;
    unsafe {
        vm.set_load_globals(true);
        let result;
        if luaL_loadbuffer(l, body.as_ptr().cast(), body.len(), c"@user_function".as_ptr()) != 0 {
            let msg = vm::to_bytes(l, -1).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
            lua_pop(l, 1);
            result = Err(format!("Error compiling function: {msg}"));
        } else {
            let mut load = Load { functions: Vec::new(), start: Instant::now(), timeout_ms };
            vm::set_registry_ptr(l, vm::LOAD_CTX, Some(&mut load as *mut Load));
            roll_sethook(l, Some(vm::hook_check));
            if lua_pcall(l, 0, 0, 0) != 0 {
                let info = vm::error_info(l);
                lua_pop(l, 1);
                for f in &load.functions {
                    luaL_unref(l, LUA_REGISTRYINDEX, f.lua_ref);
                }
                result = Err(format!("Error registering functions: {}", String::from_utf8_lossy(&info.msg)));
            } else {
                result = Ok(std::mem::take(&mut load.functions));
            }
            roll_sethook(l, None);
            vm::set_registry_ptr::<Load>(l, vm::LOAD_CTX, None);
        }
        vm.set_load_globals(false);
        vm.gc_tick();
        result.map_err(|e| e.replace(['\r', '\n'], " "))
    }
}

// ---- running ----

/// A script invocation, seen by the `redis.*` callbacks.
pub(crate) struct Run {
    cx: *mut Cx<'static>,
    /// The client the script's commands run as.
    pub client: Client,
    read_only: bool,
    start: Instant,
    threshold_ms: u64,
    timed_out: bool,
    ctl: Arc<ScriptCtl>,
    /// The script returned an error reply that a command already counted in the error stats.
    pub error_counted: bool,
}

impl Run {
    fn cx(&mut self) -> &mut Cx<'static> {
        // SAFETY: the Cx outlives the run (it's the caller's frame) and isn't otherwise used while
        // Lua runs.
        unsafe { &mut *self.cx }
    }

    /// redis.call: run a command as the script client.
    pub fn call(&mut self, args: &[Bytes]) -> Reply {
        let cmd = match script_lookup(args) {
            Ok(c) => c,
            Err(e) => {
                let r = Reply::error(e);
                self.cx().eng.count_error(&r);
                return r;
            }
        };
        if cmd.has(NOSCRIPT) && !self.cx().eng.debug.disable_deny_scripts {
            let name = Engine::resolve(args).map(|(_, n)| n).unwrap_or_default();
            return self.cx().eng.reject(&name, Reply::error("ERR This Redis command is not allowed from script"));
        }
        let read_only = self.read_only;
        let mut client = std::mem::replace(&mut self.client, Client::new(0));
        let cx = self.cx();
        let now = cx.now;
        let out = cx.eng.dispatch(&mut client, args, now, Mode::Script { read_only });
        self.client = client;
        match out {
            crate::engine::Outcome::Reply(r) => r,
            crate::engine::Outcome::Blocked => Reply::Nil,
        }
    }

    pub fn set_repl(&mut self, on: bool) {
        if let Some(r) = self.cx().eng.scripting.running.as_mut() {
            r.propagate = on;
        }
    }

    /// redis.acl_check_cmd: may the caller's user run this?
    pub fn acl_check(&mut self, args: &[Bytes]) -> Result<bool, &'static str> {
        let cmd = match crate::engine::Engine::resolve(args) {
            Ok((c, _)) => c,
            Err(Reply::Error(e)) if e.starts_with("ERR wrong number") => return Err("Wrong number of args for redis.acl_check_cmd()"),
            Err(_) => return Err("Invalid command passed to redis.acl_check_cmd()"),
        };
        let user = self.cx().client.user.clone();
        Ok(user.check(cmd, args).is_ok())
    }

    /// Called every 100k instructions: past the busy threshold the script becomes killable.
    pub fn interrupted(&mut self) -> bool {
        if !self.timed_out {
            if (self.start.elapsed().as_millis() as u64) < self.threshold_ms {
                return false;
            }
            self.timed_out = true;
            self.ctl.busy.store(true, Ordering::SeqCst);
        }
        self.ctl.kill.load(Ordering::SeqCst)
    }
}

/// Command lookup for scripts, with their error messages.
fn script_lookup(args: &[Bytes]) -> Result<&'static Command, &'static str> {
    let unknown = "ERR Unknown Redis command called from script";
    let mut cmd = lookup(&args[0]).ok_or(unknown)?;
    if !cmd.subs.is_empty() && args.len() > 1 {
        cmd = lookup_sub(cmd, &args[1]).ok_or(unknown)?;
    }
    if !cmd.check_arity(args.len()) {
        return Err("ERR Wrong number of args calling Redis command from script");
    }
    Ok(cmd)
}

/// The client a script's commands run as: the caller's user and database, RESP2 until the
/// script asks otherwise, no transaction or subscriptions of its own.
fn script_client(caller: &Client) -> Client {
    let mut c = caller.clone();
    c.resp = 2;
    c.multi = None;
    c.subs.clear();
    c.psubs.clear();
    c.ssubs.clear();
    c.monitor = false;
    c.reply_mode = ReplyMode::On;
    c.addr = "lua".into();
    // The script's client has no output buffer limit (it is not a connection).
    c.reply_budget = u64::MAX;
    // HIMPORT fieldsets are scoped to one script run.
    c.himport = None;
    c
}

/// Checks before running a script with these flags (`scriptPrepareForRun`). Returns
/// `(read_only, allow_oom)`.
fn prepare(cx: &mut Cx, flags: u64, ro: bool) -> Result<(bool, bool), Error> {
    let compat = flags & COMPAT != 0;
    let internal = cx.client.internal;
    if !compat {
        if flags & NO_WRITES == 0 {
            if cx.eng.replica && !internal {
                return Err(Error::new("READONLY Can not run script with write flag on readonly replica"));
            }
            if ro {
                return Err(Error::new("ERR Can not execute a script with write flag using *_ro command."));
            }
        }
        if !internal && cx.eng.cfg.maxmemory > 0 && cx.eng.pre_oom && flags & (ALLOW_OOM | NO_WRITES) == 0 {
            return Err(Error::new("OOM allow-oom flag is not set on the script, can not run it when used memory > 'maxmemory'"));
        }
    }
    let read_only = ro || (!compat && flags & NO_WRITES != 0);
    let allow_oom = internal || (!compat && flags & ALLOW_OOM != 0);
    Ok((read_only, allow_oom))
}

/// What the command flags become for a script with these flags (`scriptFlagsToCmdFlags`):
/// checked like a top-level command's (OOM, read-only replica) before anything runs.
fn check_as_command(cx: &mut Cx, flags: u64) -> Result<(), Error> {
    if flags & COMPAT != 0 || cx.mode != Mode::Top || cx.client.internal {
        return Ok(());
    }
    let write = flags & NO_WRITES == 0;
    let denyoom = flags & (ALLOW_OOM | NO_WRITES) == 0;
    if denyoom && cx.eng.cfg.maxmemory > 0 && cx.eng.pre_oom {
        return Err(OOM);
    }
    if write && cx.eng.replica {
        return Err(Error::new("READONLY You can't write against a read only replica."));
    }
    Ok(())
}

fn numkeys(args: &[Bytes], bad: Error) -> Result<usize, Error> {
    let n = crate::util::parse_i64(&args[2]).ok_or(bad)?;
    if n > args.len() as i64 - 3 {
        return Err(Error::new("ERR Number of keys can't be greater than number of args"));
    }
    if n < 0 {
        return Err(Error::new("ERR Number of keys can't be negative"));
    }
    Ok(n as usize)
}

unsafe fn push_array(l: *mut lua_State, items: &[Bytes]) {
    unsafe {
        lua_createtable(l, items.len() as c_int, 0);
        for (i, a) in items.iter().enumerate() {
            vm::push_bytes(l, a);
            lua_rawseti(l, -2, i as c_int + 1);
        }
    }
}

/// Run the function on the stack (above its error handler) with `keys` and `argv`: as KEYS / ARGV
/// globals for EVAL, as its two arguments for functions. Pops both.
#[allow(clippy::too_many_arguments)]
unsafe fn invoke(cx: &mut Cx, l: *mut lua_State, cmd: &[Bytes], nkeys: usize, name: &str, eval: bool, read_only: bool, allow_oom: bool) -> CmdResult {
    let (keys, argv) = (&cmd[3..3 + nkeys], &cmd[3 + nkeys..]);
    let start_effects = cx.eng.effects.len();
    let ctl = cx.eng.scripting.ctl.clone();
    ctl.kill.store(false, Ordering::SeqCst);
    ctl.dirty.store(false, Ordering::SeqCst);
    ctl.eval.store(eval, Ordering::SeqCst);
    ctl.obey.store(cx.client.internal, Ordering::SeqCst);
    *ctl.info.lock() = Some((name.to_string(), cmd.to_vec(), Instant::now()));
    cx.eng.scripting.running = Some(Running { allow_oom, write_dirty: false, propagate: true });
    let threshold_ms = cx.eng.cfg.busy_reply_threshold;
    let caller_resp3 = cx.client.resp == 3;
    let mut run = Run {
        cx: (cx as *mut Cx<'_>).cast::<Cx<'static>>(),
        client: script_client(cx.client),
        read_only,
        start: Instant::now(),
        threshold_ms,
        timed_out: false,
        ctl: ctl.clone(),
        error_counted: false,
    };
    let reply = unsafe {
        vm::set_registry_ptr(l, vm::RUN_CTX, Some(&mut run as *mut Run));
        if threshold_ms > 0 {
            roll_sethook(l, Some(vm::hook_check));
        }
        push_array(l, keys);
        if eval {
            lua_enablereadonlytable(l, LUA_GLOBALSINDEX, 0);
            lua_setglobal(l, c"KEYS".as_ptr());
            lua_enablereadonlytable(l, LUA_GLOBALSINDEX, 1);
        }
        push_array(l, argv);
        if eval {
            lua_enablereadonlytable(l, LUA_GLOBALSINDEX, 0);
            lua_setglobal(l, c"ARGV".as_ptr());
            lua_enablereadonlytable(l, LUA_GLOBALSINDEX, 1);
        }
        let rc = if eval { lua_pcall(l, 0, 1, -2) } else { lua_pcall(l, 2, 1, -4) };
        let reply = if rc != 0 {
            let r = if lua_type(l, -1) != LUA_TTABLE {
                let msg = vm::to_bytes(l, -1).map(|b| String::from_utf8_lossy(&b[..b.len().min(100)]).into_owned()).unwrap_or_else(|| "execution failure".into());
                Reply::Error(format!("ERR Error running script {name}, {msg}").replace(['\r', '\n'], " ").trim_end().to_string())
            } else {
                let info = vm::error_info(l);
                run.error_counted = info.counted;
                let mut msg = String::from_utf8_lossy(&info.msg).into_owned();
                if let (Some(line), Some(source)) = (&info.line, &info.source) {
                    msg.push_str(&format!(" script: {name}, on {}:{}.", String::from_utf8_lossy(source), String::from_utf8_lossy(line)));
                }
                Reply::Error(msg.replace(['\r', '\n'], " "))
            };
            lua_pop(l, 1);
            r
        } else {
            vm::pop_reply(l, run.client.resp == 3, caller_resp3)
        };
        lua_pop(l, 1); // the error handler
        if threshold_ms > 0 {
            roll_sethook(l, None);
        }
        vm::set_registry_ptr::<Run>(l, vm::RUN_CTX, None);
        reply
    };
    let counted = run.error_counted;
    drop(run);
    cx.eng.scripting.running = None;
    ctl.busy.store(false, Ordering::SeqCst);
    *ctl.info.lock() = None;
    crate::commands::multi::wrap_effects(cx, start_effects);
    cx.no_propagate();
    match reply {
        // Already counted by the command that failed: returned as a plain reply so it isn't again.
        Reply::Error(_) if counted => Ok(reply),
        Reply::Error(e) => Err(Error::msg(e)),
        r => Ok(r),
    }
}

// ---- EVAL ----

fn eval(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    eval_generic(cx, args, false, false)
}

fn evalsha(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    eval_generic(cx, args, true, false)
}

fn eval_ro(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    eval_generic(cx, args, false, true)
}

fn evalsha_ro(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    eval_generic(cx, args, true, true)
}

const NOSCRIPT_ERR: Error = Error::new("NOSCRIPT No matching script. Please use EVAL.");

fn feed_monitor(cx: &mut Cx, args: &[Bytes]) {
    if cx.eng.hub.has_monitors() {
        let now = cx.now;
        cx.eng.feed_monitor(cx.client, args, now);
    }
}

fn eval_generic(cx: &mut Cx, args: &[Bytes], by_sha: bool, ro: bool) -> CmdResult {
    feed_monitor(cx, args);
    if by_sha && args[1].len() != 40 {
        return Err(NOSCRIPT_ERR);
    }
    let nkeys = numkeys(args, NOT_INT)?;
    let sha = if by_sha { String::from_utf8_lossy(&args[1]).to_ascii_lowercase() } else { sha1_hex(&args[1]) };
    let flags = match cx.eng.scripting.scripts.get(&sha) {
        Some(s) => s.flags,
        None if by_sha => COMPAT,
        None => shebang_flags(&args[1]).map_or(COMPAT, |(f, _)| f),
    };
    check_as_command(cx, flags)?;

    let l = cx.eng.scripting.eval_vm().l;
    let fname = std::ffi::CString::new(format!("f_{sha}")).unwrap();
    unsafe {
        lua_getglobal(l, c"__redis__err__handler".as_ptr());
        lua_getfield(l, LUA_REGISTRYINDEX, fname.as_ptr());
        if lua_type(l, -1) == LUA_TNIL {
            lua_pop(l, 1);
            if by_sha {
                lua_pop(l, 1);
                return Err(NOSCRIPT_ERR);
            }
            let mut evicted = 0;
            let created = cx.eng.scripting.create_script(&args[1], true, &mut evicted);
            cx.eng.stats.evicted_scripts += evicted;
            if let Err(e) = created {
                lua_pop(l, 1);
                return Err(e);
            }
            lua_getfield(l, LUA_REGISTRYINDEX, fname.as_ptr());
        }
    }
    let flags = cx.eng.scripting.scripts.get(&sha).map_or(COMPAT, |s| s.flags);
    let (read_only, allow_oom) = match prepare(cx, flags, ro) {
        Ok(p) => p,
        Err(e) => {
            unsafe { lua_pop(l, 2) };
            return Err(e);
        }
    };
    cx.eng.scripting.touch_script(&sha);
    let reply = unsafe { invoke(cx, l, args, nkeys, &sha, true, read_only, allow_oom) };
    cx.eng.scripting.eval_vm().gc_tick();
    reply
}

// ---- SCRIPT ----

fn script_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "SCRIPT <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "DEBUG (YES|SYNC|NO)",
        "    Set the debug mode for subsequent scripts executed.",
        "EXISTS <sha1> [<sha1> ...]",
        "    Return information about the existence of the scripts in the script cache.",
        "FLUSH [ASYNC|SYNC]",
        "    Flush the Lua scripts cache. Very dangerous on replicas.",
        "    When called without the optional mode argument, the behavior is determined by the",
        "    lazyfree-lazy-user-flush configuration directive. Valid modes are:",
        "    * ASYNC: Asynchronously flush the scripts cache.",
        "    * SYNC: Synchronously flush the scripts cache.",
        "KILL",
        "    Kill the currently executing Lua script.",
        "LOAD <script>",
        "    Load a script into the scripts cache without executing it.",
        "HELP",
        "    Print this help.",
    ]))
}

fn flush_mode(args: &[Bytes], err: &'static str) -> Result<(), Error> {
    match args.len() {
        2 => Ok(()),
        3 if eq(&args[2], "sync") || eq(&args[2], "async") => Ok(()),
        _ => Err(Error::new(err)),
    }
}

fn script_flush(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    flush_mode(args, "ERR SCRIPT FLUSH only support SYNC|ASYNC option")?;
    cx.eng.scripting.flush_scripts();
    Ok(Reply::Ok)
}

fn script_exists(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let s = &cx.eng.scripting.scripts;
    Ok(Reply::Array(args[2..].iter().map(|a| Reply::Int(s.contains_key(&String::from_utf8_lossy(a).to_ascii_lowercase()) as i64)).collect()))
}

fn script_load(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut evicted = 0;
    let sha = cx.eng.scripting.create_script(&args[2], false, &mut evicted)?;
    Ok(Reply::bulk(sha))
}

fn script_kill(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    // Reached only when no script runs: a busy script is killed from the server layer.
    Err(Error::new("NOTBUSY No scripts in execution right now."))
}

fn script_debug(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if eq(&args[2], "no") {
        Ok(Reply::Ok)
    } else if eq(&args[2], "yes") || eq(&args[2], "sync") {
        Err(Error::new("ERR The Lua debugger is not available on roll"))
    } else {
        Err(Error::new("ERR Use SCRIPT DEBUG YES/SYNC/NO"))
    }
}

// ---- FCALL ----

fn fcall(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fcall_generic(cx, args, false)
}

fn fcall_ro(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fcall_generic(cx, args, true)
}

fn fcall_generic(cx: &mut Cx, args: &[Bytes], ro: bool) -> CmdResult {
    feed_monitor(cx, args);
    let s = &cx.eng.scripting;
    let Some(&(li, fi)) = s.fn_index.get(&String::from_utf8_lossy(&args[1]).to_ascii_lowercase()) else {
        return Err(Error::new("ERR Function not found"));
    };
    let f = &s.libs[li].functions[fi];
    let (flags, lua_ref, name) = (f.flags, f.lua_ref, f.name.clone());
    check_as_command(cx, flags)?;
    let nkeys = numkeys(args, Error::new("ERR Bad number of keys provided"))?;
    let (read_only, allow_oom) = prepare(cx, flags, ro)?;
    let l = cx.eng.scripting.fn_vm().l;
    unsafe {
        lua_getfield(l, LUA_REGISTRYINDEX, vm::ERROR_HANDLER.as_ptr());
        lua_rawgeti(l, LUA_REGISTRYINDEX, lua_ref);
    }
    let reply = unsafe { invoke(cx, l, args, nkeys, &name, false, read_only, allow_oom) };
    cx.eng.scripting.fn_vm().gc_tick();
    reply
}

// ---- FUNCTION ----

fn function_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "FUNCTION <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "LOAD [REPLACE] <FUNCTION CODE>",
        "    Create a new library with the given library name and code.",
        "DELETE <LIBRARY NAME>",
        "    Delete the given library.",
        "LIST [LIBRARYNAME PATTERN] [WITHCODE]",
        "    Return general information on all the libraries:",
        "    * Library name",
        "    * The engine used to run the Library",
        "    * Functions list",
        "    * Library code (if WITHCODE is given)",
        "    It also possible to get only function that matches a pattern using LIBRARYNAME argument.",
        "STATS",
        "    Return information about the current function running:",
        "    * Function name",
        "    * Command used to run the function",
        "    * Duration in MS that the function is running",
        "    If no function is running, return nil",
        "    In addition, returns a list of available engines.",
        "KILL",
        "    Kill the current running function.",
        "FLUSH [ASYNC|SYNC]",
        "    Delete all the libraries.",
        "    When called without the optional mode argument, the behavior is determined by the",
        "    lazyfree-lazy-user-flush configuration directive. Valid modes are:",
        "    * ASYNC: Asynchronously flush the libraries.",
        "    * SYNC: Synchronously flush the libraries.",
        "DUMP",
        "    Return a serialized payload representing the current libraries, can be restored using FUNCTION RESTORE command",
        "RESTORE <PAYLOAD> [FLUSH|APPEND|REPLACE]",
        "    Restore the libraries represented by the given payload, it is possible to give a restore policy to",
        "    control how to handle existing libraries (default APPEND):",
        "    * FLUSH: delete all existing libraries.",
        "    * APPEND: appends the restored libraries to the existing libraries. On collision, abort.",
        "    * REPLACE: appends the restored libraries to the existing libraries, On collision, replace the old",
        "      libraries with the new libraries (notice that even on this option there is a chance of failure",
        "      in case of functions name collision with another library).",
        "HELP",
        "    Print this help.",
    ]))
}

/// Mark the dataset changed so the command replicates and persists.
fn changed(cx: &mut Cx) {
    cx.eng.sig.dirty += 1;
}

fn function_load(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut replace = false;
    let mut pos = 2;
    while pos < args.len() - 1 {
        if eq(&args[pos], "replace") {
            replace = true;
            pos += 1;
            continue;
        }
        return Err(Error::msg(format!("ERR Unknown option given: {}", String::from_utf8_lossy(&args[pos]))));
    }
    let timeout = if cx.client.internal || cx.mode == Mode::Replay { 0 } else { LOAD_TIMEOUT_MS };
    let name = cx.eng.scripting.load_library(&args[pos], replace, timeout).map_err(|e| Error::msg(format!("ERR {e}")))?;
    changed(cx);
    Ok(Reply::bulk(name))
}

fn function_delete(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !cx.eng.scripting.delete_library(&args[2]) {
        return Err(Error::new("ERR Library not found"));
    }
    changed(cx);
    Ok(Reply::Ok)
}

fn function_flush(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 3 {
        return Err(Error::msg(format!(
            "ERR unknown subcommand or wrong number of arguments for '{}'. Try FUNCTION HELP.",
            String::from_utf8_lossy(&args[1])
        )));
    }
    flush_mode(args, "ERR FUNCTION FLUSH only supports SYNC|ASYNC option")?;
    cx.eng.scripting.flush_functions();
    changed(cx);
    Ok(Reply::Ok)
}

fn function_dump(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Bulk(crate::rdb::dump_functions(cx.eng.scripting.libs.iter().map(|l| &l.code[..])).into()))
}

fn function_restore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 4 {
        return Err(Error::msg(format!(
            "ERR unknown subcommand or wrong number of arguments for '{}'. Try FUNCTION HELP.",
            String::from_utf8_lossy(&args[1])
        )));
    }
    let policy = match args.get(3) {
        None => RestorePolicy::Append,
        Some(p) if eq(p, "append") => RestorePolicy::Append,
        Some(p) if eq(p, "replace") => RestorePolicy::Replace,
        Some(p) if eq(p, "flush") => RestorePolicy::Flush,
        Some(_) => return Err(Error::new("ERR Wrong restore policy given, value should be either FLUSH, APPEND or REPLACE.")),
    };
    let codes = crate::rdb::restore_functions(&args[2], cx.eng.debug.skip_checksum).map_err(|e| match e {
        crate::rdb::FunctionsPayloadError::Footer => Error::new("ERR DUMP payload version or checksum are wrong"),
        crate::rdb::FunctionsPayloadError::Message(m) => Error::msg(format!("ERR {m}")),
    })?;
    cx.eng.scripting.restore_libraries(&codes, policy).map_err(|e| Error::msg(format!("ERR {e}")))?;
    changed(cx);
    Ok(Reply::Ok)
}

fn function_kill(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Err(Error::new("NOTBUSY No scripts in execution right now."))
}

fn function_stats(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(stats(&cx.eng.scripting.ctl))
}

fn stats(ctl: &ScriptCtl) -> Reply {
    Reply::Map(vec![
        (Reply::bulk("running_script"), running_script(ctl)),
        (
            Reply::bulk("engines"),
            Reply::Map(vec![(
                Reply::bulk("LUA"),
                Reply::Map(vec![
                    (Reply::bulk("libraries_count"), Reply::Int(ctl.libraries.load(Ordering::SeqCst) as i64)),
                    (Reply::bulk("functions_count"), Reply::Int(ctl.functions.load(Ordering::SeqCst) as i64)),
                ]),
            )]),
        ),
    ])
}

fn function_list(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (mut with_code, mut pattern): (bool, Option<&Bytes>) = (false, None);
    let mut i = 2;
    while i < args.len() {
        if !with_code && eq(&args[i], "withcode") {
            with_code = true;
        } else if pattern.is_none() && eq(&args[i], "libraryname") {
            if i >= args.len() - 1 {
                return Err(Error::new("ERR library name argument was not given"));
            }
            i += 1;
            pattern = Some(&args[i]);
        } else {
            return Err(Error::msg(format!("ERR Unknown argument {}", String::from_utf8_lossy(&args[i]))));
        }
        i += 1;
    }
    let mut out = Vec::new();
    for lib in &cx.eng.scripting.libs {
        if pattern.is_some_and(|p| !crate::util::glob_match(p, lib.name.as_bytes(), true)) {
            continue;
        }
        let functions = lib
            .functions
            .iter()
            .map(|f| {
                Reply::Map(vec![
                    (Reply::bulk("name"), Reply::bulk(&f.name)),
                    (Reply::bulk("description"), f.desc.as_ref().map_or(Reply::Nil, Reply::bulk)),
                    (
                        Reply::bulk("flags"),
                        Reply::Set(FLAG_NAMES.iter().filter(|(fl, _)| f.flags & fl != 0).map(|(_, n)| Reply::status(*n)).collect()),
                    ),
                ])
            })
            .collect();
        let mut m = vec![
            (Reply::bulk("library_name"), Reply::bulk(&lib.name)),
            (Reply::bulk("engine"), Reply::bulk("LUA")),
            (Reply::bulk("functions"), Reply::Array(functions)),
        ];
        if with_code {
            m.push((Reply::bulk("library_code"), Reply::Bulk(lib.code.clone())));
        }
        out.push(Reply::Map(m));
    }
    Ok(Reply::Array(out))
}

/// FUNCTION STATS' `running_script`: the running function, if any.
fn running_script(ctl: &ScriptCtl) -> Reply {
    if ctl.eval.load(Ordering::SeqCst) {
        return Reply::Nil;
    }
    match &*ctl.info.lock() {
        Some((name, args, start)) => Reply::Map(vec![
            (Reply::bulk("name"), Reply::bulk(name)),
            (Reply::bulk("command"), Reply::Array(args.iter().cloned().map(Reply::Bulk).collect())),
            (Reply::bulk("duration_ms"), Reply::Int(start.elapsed().as_millis() as i64)),
        ]),
        None => Reply::Nil,
    }
}

const BUSY_EVAL: &str = "BUSY Redis is busy running a script. You can only call SCRIPT KILL or SHUTDOWN NOSAVE.";
const BUSY_FUNCTION: &str = "BUSY Redis is busy running a script. You can only call FUNCTION KILL or SHUTDOWN NOSAVE.";

/// The reply to a command from another connection while a script runs past
/// `busy-reply-threshold` (`ctl.busy`): SCRIPT KILL, FUNCTION KILL and FUNCTION STATS work,
/// everything else is answered BUSY.
pub fn while_busy(ctl: &ScriptCtl, args: &[Bytes]) -> Reply {
    let is_eval = ctl.eval.load(Ordering::SeqCst);
    let sub = |name: &str, sub: &str| args.len() == 2 && eq(&args[0], name) && eq(&args[1], sub);
    let kill_eval = sub("script", "kill");
    if kill_eval || sub("function", "kill") {
        if ctl.obey.load(Ordering::SeqCst) {
            return Reply::error("UNKILLABLE The busy script was sent by a master instance in the context of replication and cannot be killed.");
        }
        if ctl.dirty.load(Ordering::SeqCst) {
            return Reply::error("UNKILLABLE Sorry the script already executed write commands against the dataset. You can either wait the script termination or kill the server in a hard way using the SHUTDOWN NOSAVE command.");
        }
        if kill_eval && !is_eval {
            return Reply::error(BUSY_FUNCTION);
        }
        if !kill_eval && is_eval {
            return Reply::error(BUSY_EVAL);
        }
        ctl.kill.store(true, Ordering::SeqCst);
        return Reply::Ok;
    }
    if sub("function", "stats") && !is_eval {
        return stats(ctl);
    }
    Reply::error(if is_eval { BUSY_EVAL } else { BUSY_FUNCTION })
}

#[cfg(test)]
mod tests;
