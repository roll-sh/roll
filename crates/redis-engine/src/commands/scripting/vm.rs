//! The Lua side of scripting: building a sandboxed state the way Redis does (two flavours, one
//! for EVAL and one for functions), the `redis.*` API, and conversions between Lua values and
//! replies.
//!
//! Lua errors are `longjmp`s that must not cross Rust frames, so every callback here signals
//! errors by returning [`RAISE`] and the C trampoline raises. Raw table writes only ever target
//! fresh tables (or tables we unlocked ourselves), which can't fail.

use super::{Load, Run};
use crate::commands::server::REDIS_VERSION;
use bytes::Bytes;
use redis_lua::*;
use redis_proto::Reply;
use std::ffi::{CStr, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(crate) const RUN_CTX: &CStr = c"__RUN_CTX__";
pub(crate) const LOAD_CTX: &CStr = c"__LIBRARY_CTX__";
const LIBRARY_API: &CStr = c"__LIBRARY_API__";
const GLOBALS_API: &CStr = c"__GLOBALS_API__";
pub(crate) const ERROR_HANDLER: &CStr = c"__ERROR_HANDLER__";

/// Collect a little garbage every this many runs (a full cycle at once would add latency).
const GC_PERIOD: u32 = 50;

/// Deeper replies than this become "reached lua stack limit" (Lua's own stack runs out at a few
/// thousand levels anyway; this keeps the Rust recursion well inside a thread's stack).
const MAX_DEPTH: usize = 1000;

/// Annotates a raised error with the position that raised it (the C caller when the error comes
/// from a C function such as redis.call).
const ERROR_HANDLER_BODY: &str = "  local i = dbg.getinfo(2,'nSl')\n  if i and i.what == 'C' then\n    i = dbg.getinfo(3,'nSl')\n  end\n  if type(err) ~= 'table' then\n    err = {err='ERR ' .. tostring(err)}\n  end\n  if i then\n    err['source'] = i.source\n    err['line'] = i.currentline\n  end\n  return err\n";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Eval,
    Functions,
}

pub(crate) struct Vm {
    pub l: *mut lua_State,
    gc_count: u32,
}

// The state is only touched while the owning engine is locked.
unsafe impl Send for Vm {}

impl Drop for Vm {
    fn drop(&mut self) {
        unsafe {
            lua_gc(self.l, LUA_GCCOLLECT, 0);
            lua_close(self.l);
        }
    }
}

impl Vm {
    pub fn new(flavor: Flavor) -> Vm {
        unsafe {
            let l = roll_newstate();
            assert!(!l.is_null(), "creating the Lua state");
            // Until the globals are locked, only allow-listed globals can be created.
            lua_pushvalue(l, LUA_GLOBALSINDEX);
            roll_set_allowlist(l, 0);
            lua_pop(l, 1);
            roll_open_libs(l);
            roll_push_pcall(l);
            lua_setglobal(l, c"pcall".as_ptr());

            lua_newtable(l);
            set_fn(l, c"call", redis_call);
            set_fn(l, c"pcall", redis_pcall);
            register_log(l);
            register_version(l);
            set_fn(l, c"setresp", set_resp);
            set_fn(l, c"sha1hex", sha1hex);
            set_fn(l, c"error_reply", error_reply);
            set_fn(l, c"status_reply", status_reply);
            set_fn(l, c"set_repl", set_repl);
            set_num(l, c"REPL_NONE", 0.0);
            set_num(l, c"REPL_AOF", 1.0);
            set_num(l, c"REPL_SLAVE", 2.0);
            set_num(l, c"REPL_REPLICA", 2.0);
            set_num(l, c"REPL_ALL", 3.0);
            set_fn(l, c"acl_check_cmd", acl_check_cmd);
            if flavor == Flavor::Eval {
                set_fn(l, c"breakpoint", breakpoint);
                set_fn(l, c"debug", debug);
                set_fn(l, c"replicate_commands", replicate_commands);
            }
            lua_setglobal(l, c"redis".as_ptr());

            lua_getglobal(l, c"math".as_ptr());
            roll_set_random(l);
            lua_setglobal(l, c"math".as_ptr());

            match flavor {
                Flavor::Eval => {
                    let src = format!("local dbg = debug\ndebug = nil\nfunction __redis__err__handler(err)\n{ERROR_HANDLER_BODY}end\n");
                    load_and_run(l, &src, 0);
                    lua_pushvalue(l, LUA_GLOBALSINDEX);
                    roll_set_error_metatable(l);
                    roll_protect(l);
                    lua_pop(l, 1);
                }
                Flavor::Functions => {
                    // What library code sees while FUNCTION LOAD runs it: only `redis` with
                    // register_function, log and the version.
                    lua_newtable(l);
                    lua_newtable(l);
                    set_fn(l, c"register_function", register_function);
                    register_log(l);
                    register_version(l);
                    roll_set_error_metatable(l);
                    lua_setfield(l, -2, c"redis".as_ptr());
                    roll_set_error_metatable(l);
                    roll_protect(l);
                    lua_setfield(l, LUA_REGISTRYINDEX, LIBRARY_API.as_ptr());

                    lua_pushstring(l, ERROR_HANDLER.as_ptr());
                    let src = format!("local dbg = debug\ndebug = nil\nlocal error_handler = function (err)\n{ERROR_HANDLER_BODY}end\nreturn error_handler");
                    load_and_run(l, &src, 1);
                    lua_settable(l, LUA_REGISTRYINDEX);

                    lua_pushvalue(l, LUA_GLOBALSINDEX);
                    roll_set_error_metatable(l);
                    roll_protect(l);
                    lua_pop(l, 1);
                    lua_pushvalue(l, LUA_GLOBALSINDEX);
                    lua_setfield(l, LUA_REGISTRYINDEX, GLOBALS_API.as_ptr());

                    // Functions run with an empty, locked globals table that reads through to
                    // the real globals (or, while loading, to the library API).
                    lua_newtable(l);
                    lua_newtable(l);
                    lua_pushvalue(l, LUA_GLOBALSINDEX);
                    lua_setfield(l, -2, c"__index".as_ptr());
                    lua_enablereadonlytable(l, -1, 1);
                    lua_setmetatable(l, -2);
                    lua_enablereadonlytable(l, -1, 1);
                    lua_replace(l, LUA_GLOBALSINDEX);
                }
            }
            roll_protect_basic_types(l);
            Vm { l, gc_count: 0 }
        }
    }

    /// Bytes the state uses (`used_memory_vm_*`).
    pub fn memory(&self) -> usize {
        unsafe { lua_gc(self.l, LUA_GCCOUNT, 0) as usize * 1024 }
    }

    pub fn gc_tick(&mut self) {
        self.gc_count += 1;
        if self.gc_count >= GC_PERIOD {
            unsafe { lua_gc(self.l, LUA_GCSTEP, GC_PERIOD as c_int) };
            self.gc_count = 0;
        }
    }

    /// Switch what library code sees as globals: the library API while loading, the regular
    /// globals otherwise.
    pub unsafe fn set_load_globals(&self, loading: bool) {
        let l = self.l;
        unsafe {
            lua_getmetatable(l, LUA_GLOBALSINDEX);
            lua_enablereadonlytable(l, -1, 0);
            lua_getfield(l, LUA_REGISTRYINDEX, if loading { LIBRARY_API.as_ptr() } else { GLOBALS_API.as_ptr() });
            lua_setfield(l, -2, c"__index".as_ptr());
            lua_enablereadonlytable(l, -1, 1);
            lua_pop(l, 1);
        }
    }
}

unsafe fn load_and_run(l: *mut lua_State, src: &str, results: c_int) {
    unsafe {
        let rc = luaL_loadbuffer(l, src.as_ptr().cast(), src.len(), c"@err_handler_def".as_ptr());
        assert_eq!(rc, 0, "error handler compiles");
        assert_eq!(lua_pcall(l, 0, results, 0), 0, "error handler runs");
    }
}

unsafe fn set_fn(l: *mut lua_State, name: &CStr, f: roll_fn) {
    unsafe {
        roll_pushfn(l, f);
        lua_setfield(l, -2, name.as_ptr());
    }
}

unsafe fn set_num(l: *mut lua_State, name: &CStr, v: f64) {
    unsafe {
        lua_pushnumber(l, v);
        lua_setfield(l, -2, name.as_ptr());
    }
}

unsafe fn register_log(l: *mut lua_State) {
    unsafe {
        set_fn(l, c"log", log);
        set_num(l, c"LOG_DEBUG", 0.0);
        set_num(l, c"LOG_VERBOSE", 1.0);
        set_num(l, c"LOG_NOTICE", 2.0);
        set_num(l, c"LOG_WARNING", 3.0);
    }
}

pub(crate) fn version_num() -> f64 {
    let mut n = 0u32;
    for part in REDIS_VERSION.split('.').take(3) {
        n = (n << 8) | part.parse::<u32>().unwrap_or(0);
    }
    n as f64
}

unsafe fn register_version(l: *mut lua_State) {
    unsafe {
        set_num(l, c"REDIS_VERSION_NUM", version_num());
        push_bytes(l, REDIS_VERSION.as_bytes());
        lua_setfield(l, -2, c"REDIS_VERSION".as_ptr());
    }
}

// ---- stack helpers ----

pub(crate) unsafe fn push_bytes(l: *mut lua_State, b: &[u8]) {
    unsafe { lua_pushlstring(l, b.as_ptr().cast(), b.len()) }
}

/// The string at `idx` (numbers are converted in place, as `lua_tolstring` does).
pub(crate) unsafe fn to_bytes<'a>(l: *mut lua_State, idx: c_int) -> Option<&'a [u8]> {
    unsafe {
        let mut len = 0;
        let p = lua_tolstring(l, idx, &mut len);
        if p.is_null() { None } else { Some(std::slice::from_raw_parts(p.cast(), len)) }
    }
}

/// Set `t[name] = <stack top>` on the fresh table below the value.
unsafe fn raw_field(l: *mut lua_State, name: &[u8]) {
    unsafe {
        push_bytes(l, name);
        lua_insert(l, -2);
        lua_rawset(l, -3);
    }
}

/// Push a Redis error table (`{err = "CODE message"}`). `e` is either `-CODE message` (the code
/// is split off at the first space) or a bare message, which gets the `ERR` code.
pub(crate) unsafe fn push_error(l: *mut lua_State, e: &[u8]) {
    let (code, msg): (&[u8], &[u8]) = match e.strip_prefix(b"-") {
        Some(rest) => match rest.iter().position(|&c| c == b' ') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (b"ERR", rest),
        },
        None => (b"ERR", e),
    };
    let msg = trim_crlf(msg);
    let mut full = Vec::with_capacity(code.len() + 1 + msg.len());
    full.extend_from_slice(code);
    full.push(b' ');
    full.extend_from_slice(msg);
    // C strings end at a NUL, as Redis' error tables do.
    if let Some(nul) = full.iter().position(|&c| c == 0) {
        full.truncate(nul);
    }
    unsafe {
        lua_newtable(l);
        push_bytes(l, &full);
        raw_field(l, b"err");
    }
}

fn trim_crlf(mut s: &[u8]) -> &[u8] {
    while let [b'\r' | b'\n', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b'\r' | b'\n'] = s {
        s = rest;
    }
    s
}

/// Mark the error table on the stack top as already counted in the error stats.
unsafe fn mark_counted(l: *mut lua_State) {
    unsafe {
        lua_pushboolean(l, 1);
        raw_field(l, b"ignore_error_stats_update");
    }
}

/// Run a callback body, turning a panic into a raised error instead of unwinding into C.
unsafe fn guard(l: *mut lua_State, f: impl FnOnce() -> c_int) -> c_int {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(n) => n,
        Err(_) => {
            unsafe { push_error(l, b"internal error in the scripting engine") };
            RAISE
        }
    }
}

pub(crate) unsafe fn run_ctx<'a>(l: *mut lua_State) -> Option<&'a mut Run> {
    unsafe { registry_ptr::<Run>(l, RUN_CTX).map(|p| &mut *p) }
}

pub(crate) unsafe fn load_ctx<'a>(l: *mut lua_State) -> Option<&'a mut Load> {
    unsafe { registry_ptr::<Load>(l, LOAD_CTX).map(|p| &mut *p) }
}

unsafe fn registry_ptr<T>(l: *mut lua_State, name: &CStr) -> Option<*mut T> {
    unsafe {
        lua_getfield(l, LUA_REGISTRYINDEX, name.as_ptr());
        let p = if lua_type(l, -1) == LUA_TLIGHTUSERDATA { Some(lua_touserdata(l, -1).cast::<T>()) } else { None };
        lua_pop(l, 1);
        p
    }
}

pub(crate) unsafe fn set_registry_ptr<T>(l: *mut lua_State, name: &CStr, p: Option<*mut T>) {
    unsafe {
        match p {
            Some(p) => lua_pushlightuserdata(l, p.cast()),
            None => lua_pushnil(l),
        }
        lua_setfield(l, LUA_REGISTRYINDEX, name.as_ptr());
    }
}

// ---- arguments ----

/// A Lua number as a command argument: integral values print as integers (no exponent),
/// others in Redis' shortest double form.
fn number_arg(n: f64) -> Bytes {
    if n.fract() == 0.0 && (-4.611_686_018_427_388e18..=4.611_686_018_427_388e18).contains(&n) {
        Bytes::from((n as i64).to_string())
    } else {
        Bytes::from(redis_proto::fmt_double(n))
    }
}

/// Pop every argument of a redis.call-like function as command arguments. On error the error
/// table is pushed and `None` returned.
unsafe fn take_args(l: *mut lua_State) -> Option<Vec<Bytes>> {
    unsafe {
        let argc = lua_gettop(l);
        if argc == 0 {
            push_error(l, b"Please specify at least one argument for this redis lib call");
            return None;
        }
        let mut args = Vec::with_capacity(argc as usize);
        for j in 1..=argc {
            if lua_type(l, j) == LUA_TNUMBER {
                args.push(number_arg(lua_tonumber(l, j)));
            } else if lua_type(l, j) == LUA_TSTRING {
                args.push(Bytes::copy_from_slice(to_bytes(l, j).unwrap_or_default()));
            } else {
                break;
            }
        }
        lua_pop(l, argc);
        if args.len() != argc as usize {
            push_error(l, b"Lua redis lib command arguments must be strings or integers");
            return None;
        }
        Some(args)
    }
}

// ---- reply -> Lua ----

/// Push a command's reply as the script's protocol version sees it.
pub(crate) unsafe fn push_reply(l: *mut lua_State, r: &Reply, resp3: bool) {
    unsafe {
        lua_checkstack(l, 4);
        match r {
            Reply::Ok => push_single(l, b"ok", |l| push_bytes(l, b"OK")),
            Reply::Status(s) => push_single(l, b"ok", |l| push_bytes(l, s.as_bytes())),
            Reply::Error(e) => {
                let mut v = Vec::with_capacity(e.len() + 1);
                v.push(b'-');
                v.extend_from_slice(e.as_bytes());
                push_error(l, &v);
                mark_counted(l);
            }
            Reply::Int(i) => lua_pushnumber(l, *i as f64),
            Reply::Bulk(b) => push_bytes(l, b),
            Reply::Nil | Reply::NilArray => {
                if resp3 {
                    lua_pushnil(l)
                } else {
                    lua_pushboolean(l, 0)
                }
            }
            Reply::Array(items) | Reply::Push(items) | Reply::Seq(items) => push_array(l, items, resp3),
            Reply::Set(items) if !resp3 => push_array(l, items, resp3),
            Reply::Set(items) => push_single(l, b"set", |l| {
                lua_createtable(l, 0, items.len() as c_int);
                for it in items {
                    push_reply(l, it, resp3);
                    if usable_key(l) {
                        lua_pushboolean(l, 1);
                        lua_rawset(l, -3);
                    } else {
                        lua_pop(l, 1);
                    }
                }
            }),
            Reply::Map(pairs) if !resp3 => {
                lua_createtable(l, (pairs.len() * 2) as c_int, 0);
                let mut i = 1;
                for (k, v) in pairs {
                    push_reply(l, k, resp3);
                    lua_rawseti(l, -2, i);
                    push_reply(l, v, resp3);
                    lua_rawseti(l, -2, i + 1);
                    i += 2;
                }
            }
            Reply::Map(pairs) => push_single(l, b"map", |l| {
                lua_createtable(l, 0, pairs.len() as c_int);
                for (k, v) in pairs {
                    push_reply(l, k, resp3);
                    if usable_key(l) {
                        push_reply(l, v, resp3);
                        lua_rawset(l, -3);
                    } else {
                        lua_pop(l, 1);
                    }
                }
            }),
            Reply::Double(d) if !resp3 => push_bytes(l, redis_proto::fmt_double(*d).as_bytes()),
            Reply::Double(d) => push_single(l, b"double", |l| lua_pushnumber(l, *d)),
            Reply::Bool(b) if !resp3 => lua_pushnumber(l, if *b { 1.0 } else { 0.0 }),
            Reply::Bool(b) => lua_pushboolean(l, *b as c_int),
            Reply::BigNum(n) if !resp3 => push_bytes(l, n.as_bytes()),
            Reply::BigNum(n) => push_single(l, b"big_number", |l| push_bytes(l, n.as_bytes())),
            Reply::Verbatim(_, b) if !resp3 => push_bytes(l, b),
            Reply::Verbatim(fmt, b) => push_single(l, b"verbatim_string", |l| {
                lua_newtable(l);
                push_bytes(l, b);
                raw_field(l, b"string");
                push_bytes(l, fmt.as_bytes());
                raw_field(l, b"format");
            }),
            Reply::Raw(raw) => match redis_proto::decode(raw) {
                Ok(Some((r, _))) => push_reply(l, &r, resp3),
                _ => lua_pushboolean(l, 0),
            },
        }
    }
}

/// A key that can index a table (not nil, not NaN).
unsafe fn usable_key(l: *mut lua_State) -> bool {
    unsafe {
        match lua_type(l, -1) {
            LUA_TNIL => false,
            LUA_TNUMBER => !lua_tonumber(l, -1).is_nan(),
            _ => true,
        }
    }
}

unsafe fn push_single(l: *mut lua_State, field: &[u8], value: impl FnOnce(*mut lua_State)) {
    unsafe {
        lua_newtable(l);
        value(l);
        raw_field(l, field);
    }
}

unsafe fn push_array(l: *mut lua_State, items: &[Reply], resp3: bool) {
    unsafe {
        lua_createtable(l, items.len() as c_int, 0);
        for (i, it) in items.iter().enumerate() {
            push_reply(l, it, resp3);
            lua_rawseti(l, -2, i as c_int + 1);
        }
    }
}

// ---- Lua -> reply ----

/// Convert (and pop) the value on the stack top into a reply for the caller. `script_resp3` is
/// the protocol the script selected (booleans depend on it), `caller_resp3` the caller's.
pub(crate) unsafe fn pop_reply(l: *mut lua_State, script_resp3: bool, caller_resp3: bool) -> Reply {
    unsafe { pop_reply_at(l, script_resp3, caller_resp3, 0) }
}

unsafe fn pop_reply_at(l: *mut lua_State, s3: bool, c3: bool, depth: usize) -> Reply {
    unsafe {
        if depth > MAX_DEPTH || lua_checkstack(l, 4) == 0 {
            lua_pop(l, 1);
            return Reply::error("ERR reached lua stack limit");
        }
        let r = match lua_type(l, -1) {
            LUA_TSTRING => Reply::Bulk(Bytes::copy_from_slice(to_bytes(l, -1).unwrap_or_default())),
            LUA_TBOOLEAN => {
                let b = lua_toboolean(l, -1) != 0;
                if !s3 {
                    if b { Reply::Int(1) } else { Reply::Nil }
                } else {
                    Reply::Bool(b)
                }
            }
            // C's (long long) cast; arm64 saturates and maps NaN to 0, as Rust's `as` does.
            LUA_TNUMBER => Reply::Int(lua_tonumber(l, -1) as i64),
            LUA_TTABLE => return table_reply(l, s3, c3, depth),
            _ => Reply::Nil,
        };
        lua_pop(l, 1);
        r
    }
}

/// Raw-get `t[name]` (t on the stack top) and push it.
unsafe fn raw_get(l: *mut lua_State, name: &[u8]) -> c_int {
    unsafe {
        push_bytes(l, name);
        lua_rawget(l, -2);
        lua_type(l, -1)
    }
}

fn sanitize(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace(['\r', '\n'], " ")
}

unsafe fn table_reply(l: *mut lua_State, s3: bool, c3: bool, depth: usize) -> Reply {
    unsafe {
        if raw_get(l, b"err") == LUA_TSTRING {
            lua_pop(l, 1);
            let info = error_info(l);
            lua_pop(l, 1);
            if depth == 0
                && let Some(run) = run_ctx(l)
            {
                run.error_counted = info.counted;
            }
            return Reply::Error(sanitize(&info.msg));
        }
        lua_pop(l, 1);
        if raw_get(l, b"ok") == LUA_TSTRING {
            let s = sanitize(to_bytes(l, -1).unwrap_or_default());
            lua_pop(l, 2);
            return if s == "OK" { Reply::Ok } else { Reply::Status(s) };
        }
        lua_pop(l, 1);
        if raw_get(l, b"double") == LUA_TNUMBER {
            let d = lua_tonumber(l, -1);
            lua_pop(l, 2);
            return Reply::Double(d);
        }
        lua_pop(l, 1);
        if raw_get(l, b"big_number") == LUA_TSTRING {
            let n = String::from_utf8_lossy(to_bytes(l, -1).unwrap_or_default()).replace(['\r', '\n'], " ");
            lua_pop(l, 2);
            return Reply::BigNum(n);
        }
        lua_pop(l, 1);
        if raw_get(l, b"verbatim_string") == LUA_TTABLE {
            if raw_get(l, b"format") == LUA_TSTRING {
                let fmt = to_bytes(l, -1).unwrap_or_default().to_vec();
                lua_pop(l, 1);
                if raw_get(l, b"string") == LUA_TSTRING {
                    let s = Bytes::copy_from_slice(to_bytes(l, -1).unwrap_or_default());
                    lua_pop(l, 3);
                    return verbatim(&fmt, s, c3);
                }
                lua_pop(l, 1);
            } else {
                lua_pop(l, 1);
            }
        }
        lua_pop(l, 1);
        if raw_get(l, b"map") == LUA_TTABLE {
            let mut pairs = Vec::new();
            lua_pushnil(l);
            while lua_next(l, -2) != 0 {
                lua_pushvalue(l, -2);
                let k = pop_reply_at(l, s3, c3, depth + 1);
                let v = pop_reply_at(l, s3, c3, depth + 1);
                pairs.push((k, v));
            }
            lua_pop(l, 2);
            return Reply::Map(pairs);
        }
        lua_pop(l, 1);
        if raw_get(l, b"set") == LUA_TTABLE {
            let mut items = Vec::new();
            lua_pushnil(l);
            while lua_next(l, -2) != 0 {
                lua_pop(l, 1);
                lua_pushvalue(l, -1);
                items.push(pop_reply_at(l, s3, c3, depth + 1));
            }
            lua_pop(l, 2);
            return Reply::Set(items);
        }
        lua_pop(l, 1);
        let mut items = Vec::new();
        let mut j = 1;
        loop {
            lua_rawgeti(l, -1, j);
            j += 1;
            if lua_type(l, -1) == LUA_TNIL {
                lua_pop(l, 1);
                break;
            }
            items.push(pop_reply_at(l, s3, c3, depth + 1));
        }
        lua_pop(l, 1);
        Reply::Array(items)
    }
}

/// A verbatim string with any three-letter format (the reply type only names the common ones).
fn verbatim(fmt: &[u8], s: Bytes, caller_resp3: bool) -> Reply {
    match fmt {
        b"txt" => Reply::Verbatim("txt", s),
        b"mkd" => Reply::Verbatim("mkd", s),
        _ if !caller_resp3 => Reply::Bulk(s),
        _ => {
            let mut f = [b' '; 3];
            for (d, c) in f.iter_mut().zip(fmt) {
                *d = *c;
            }
            let mut out = format!("={}\r\n", s.len() + 4).into_bytes();
            out.extend_from_slice(&f);
            out.push(b':');
            out.extend_from_slice(&s);
            out.extend_from_slice(b"\r\n");
            Reply::Raw(Bytes::from(out))
        }
    }
}

pub(crate) struct ErrorInfo {
    pub msg: Vec<u8>,
    pub source: Option<Vec<u8>>,
    pub line: Option<Vec<u8>>,
    /// Came from a command's reply, which already counted it in the error stats.
    pub counted: bool,
}

/// Read a raised error (stack top): a string, or a table with err / source / line.
pub(crate) unsafe fn error_info(l: *mut lua_State) -> ErrorInfo {
    unsafe {
        if lua_isstring(l, -1) != 0 {
            let mut msg = b"ERR ".to_vec();
            msg.extend_from_slice(to_bytes(l, -1).unwrap_or_default());
            return ErrorInfo { msg, source: None, line: None, counted: false };
        }
        if lua_type(l, -1) != LUA_TTABLE {
            return ErrorInfo { msg: b"ERR unknown error".to_vec(), source: None, line: None, counted: false };
        }
        let field = |name: &[u8]| -> Option<Vec<u8>> {
            raw_get(l, name);
            let v = if lua_isstring(l, -1) != 0 { to_bytes(l, -1).map(|b| b.to_vec()) } else { None };
            lua_pop(l, 1);
            v
        };
        ErrorInfo {
            msg: field(b"err").unwrap_or_else(|| b"ERR unknown error".to_vec()),
            source: field(b"source"),
            line: field(b"line"),
            counted: {
                raw_get(l, b"ignore_error_stats_update");
                let b = lua_toboolean(l, -1) != 0;
                lua_pop(l, 1);
                b
            },
        }
    }
}

// ---- redis.* ----

unsafe extern "C" fn redis_call(l: *mut lua_State) -> c_int {
    unsafe { guard(l, || command(l, true)) }
}

unsafe extern "C" fn redis_pcall(l: *mut lua_State) -> c_int {
    unsafe { guard(l, || command(l, false)) }
}

unsafe fn command(l: *mut lua_State, raise: bool) -> c_int {
    unsafe {
        let Some(run) = run_ctx(l) else {
            push_error(l, b"redis.call can only be called inside a script invocation");
            return RAISE;
        };
        let Some(args) = take_args(l) else {
            return if raise { RAISE } else { 1 };
        };
        let reply = run.call(&args);
        push_reply(l, &reply, run.client.resp == 3);
        if raise && matches!(reply, Reply::Error(_)) { RAISE } else { 1 }
    }
}

unsafe extern "C" fn sha1hex(l: *mut lua_State) -> c_int {
    unsafe {
        guard(l, || {
            if lua_gettop(l) != 1 {
                push_error(l, b"wrong number of arguments");
                return RAISE;
            }
            let digest = super::sha1_hex(to_bytes(l, 1).unwrap_or_default());
            push_bytes(l, digest.as_bytes());
            1
        })
    }
}

unsafe fn single_string_arg(l: *mut lua_State) -> bool {
    unsafe { lua_gettop(l) == 1 && lua_type(l, -1) == LUA_TSTRING }
}

unsafe extern "C" fn error_reply(l: *mut lua_State) -> c_int {
    unsafe {
        guard(l, || {
            if !single_string_arg(l) {
                push_error(l, b"wrong number or type of arguments");
                return 1;
            }
            let e = to_bytes(l, -1).unwrap_or_default();
            // C string semantics: the message ends at a NUL.
            let e = &e[..e.iter().position(|&c| c == 0).unwrap_or(e.len())];
            if e.first() == Some(&b'-') {
                push_error(l, e);
            } else {
                let mut v = b"-".to_vec();
                v.extend_from_slice(e);
                push_error(l, &v);
            }
            1
        })
    }
}

unsafe extern "C" fn status_reply(l: *mut lua_State) -> c_int {
    unsafe {
        guard(l, || {
            if !single_string_arg(l) {
                push_error(l, b"wrong number or type of arguments");
                return 1;
            }
            lua_newtable(l);
            push_bytes(l, b"ok");
            lua_pushvalue(l, -3);
            lua_rawset(l, -3);
            1
        })
    }
}

unsafe extern "C" fn set_resp(l: *mut lua_State) -> c_int {
    unsafe {
        guard(l, || {
            let Some(run) = run_ctx(l) else { return 0 };
            if lua_gettop(l) != 1 {
                push_error(l, b"redis.setresp() requires one argument.");
                return RAISE;
            }
            let resp = lua_tonumber(l, -1) as i64;
            if resp != 2 && resp != 3 {
                push_error(l, b"RESP version must be 2 or 3.");
                return RAISE;
            }
            run.client.resp = resp as u8;
            0
        })
    }
}

unsafe extern "C" fn set_repl(l: *mut lua_State) -> c_int {
    unsafe {
        guard(l, || {
            let Some(run) = run_ctx(l) else { return 0 };
            if lua_gettop(l) != 1 {
                push_error(l, b"redis.set_repl() requires one argument.");
                return RAISE;
            }
            let flags = lua_tonumber(l, -1) as i64 as i32;
            if flags & !3 != 0 {
                push_error(l, b"Invalid replication flags. Use REPL_AOF, REPL_REPLICA, REPL_ALL or REPL_NONE.");
                return RAISE;
            }
            run.set_repl(flags != 0);
            0
        })
    }
}

unsafe extern "C" fn acl_check_cmd(l: *mut lua_State) -> c_int {
    unsafe {
        guard(l, || {
            let Some(run) = run_ctx(l) else { return 0 };
            let Some(args) = take_args(l) else { return RAISE };
            match run.acl_check(&args) {
                Ok(allowed) => {
                    lua_pushboolean(l, allowed as c_int);
                    1
                }
                Err(e) => {
                    push_error(l, e.as_bytes());
                    RAISE
                }
            }
        })
    }
}

unsafe extern "C" fn log(l: *mut lua_State) -> c_int {
    unsafe {
        guard(l, || {
            let argc = lua_gettop(l);
            if argc < 2 {
                push_error(l, b"redis.log() requires two arguments or more.");
                return RAISE;
            }
            if lua_isnumber(l, -argc) == 0 {
                push_error(l, b"First argument must be a number (log level).");
                return RAISE;
            }
            let level = lua_tonumber(l, -argc) as i64;
            if !(0..=3).contains(&level) {
                push_error(l, b"Invalid log level.");
                return RAISE;
            }
            // There is no server log to write to; the message is dropped.
            0
        })
    }
}

unsafe extern "C" fn breakpoint(l: *mut lua_State) -> c_int {
    unsafe {
        lua_pushboolean(l, 0);
        1
    }
}

unsafe extern "C" fn debug(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C" fn replicate_commands(l: *mut lua_State) -> c_int {
    unsafe {
        lua_pushboolean(l, 1);
        1
    }
}

// ---- redis.register_function (FUNCTION LOAD) ----

pub(crate) struct FnDef {
    pub name: String,
    pub desc: Option<String>,
    pub flags: u64,
    pub lua_ref: c_int,
}

unsafe extern "C" fn register_function(l: *mut lua_State) -> c_int {
    unsafe {
        guard(l, || {
            let Some(load) = load_ctx(l) else {
                push_error(l, b"redis.register_function can only be called on FUNCTION LOAD command");
                return RAISE;
            };
            let def = match lua_gettop(l) {
                1 => named_args(l),
                2 => positional_args(l),
                _ => Err("wrong number of arguments to redis.register_function"),
            };
            let def = match def {
                Ok(d) => d,
                Err(e) => {
                    push_error(l, e.as_bytes());
                    return RAISE;
                }
            };
            let err = if !super::valid_name(def.name.as_bytes()) {
                Some("Library names can only contain letters, numbers, or underscores(_) and must be at least one character long")
            } else if load.functions.iter().any(|f| f.name == def.name) {
                Some("Function already exists in the library")
            } else {
                None
            };
            if let Some(e) = err {
                luaL_unref(l, LUA_REGISTRYINDEX, def.lua_ref);
                push_error(l, e.as_bytes());
                return RAISE;
            }
            load.functions.push(def);
            0
        })
    }
}

unsafe fn string_arg(l: *mut lua_State, idx: c_int) -> Option<String> {
    unsafe {
        if lua_isstring(l, idx) == 0 {
            return None;
        }
        to_bytes(l, idx).map(|b| String::from_utf8_lossy(b).into_owned())
    }
}

unsafe fn positional_args(l: *mut lua_State) -> Result<FnDef, &'static str> {
    unsafe {
        let name = string_arg(l, 1).ok_or("first argument to redis.register_function must be a string")?;
        if lua_type(l, 2) != LUA_TFUNCTION {
            return Err("second argument to redis.register_function must be a function");
        }
        let lua_ref = luaL_ref(l, LUA_REGISTRYINDEX);
        Ok(FnDef { name, desc: None, flags: 0, lua_ref })
    }
}

unsafe fn named_args(l: *mut lua_State) -> Result<FnDef, &'static str> {
    unsafe {
        if lua_type(l, 1) != LUA_TTABLE {
            return Err("calling redis.register_function with a single argument is only applicable to Lua table (representing named arguments).");
        }
        let (mut name, mut desc, mut lua_ref, mut flags) = (None, None, None, 0u64);
        let fail = |l, lua_ref: Option<c_int>, e| {
            if let Some(r) = lua_ref {
                luaL_unref(l, LUA_REGISTRYINDEX, r);
            }
            Err(e)
        };
        lua_pushnil(l);
        while lua_next(l, -2) != 0 {
            if lua_isstring(l, -2) == 0 {
                return fail(l, lua_ref, "named argument key given to redis.register_function is not a string");
            }
            // Copy the key: converting a number key in place would break lua_next.
            lua_pushvalue(l, -2);
            let key = to_bytes(l, -1).unwrap_or_default().to_ascii_lowercase();
            lua_pop(l, 1);
            match key.as_slice() {
                b"function_name" => match string_arg(l, -1) {
                    Some(n) => name = Some(n),
                    None => return fail(l, lua_ref, "function_name argument given to redis.register_function must be a string"),
                },
                b"description" => match string_arg(l, -1) {
                    Some(d) => desc = Some(d),
                    None => return fail(l, lua_ref, "description argument given to redis.register_function must be a string"),
                },
                b"callback" => {
                    if lua_type(l, -1) != LUA_TFUNCTION {
                        return fail(l, lua_ref, "callback argument given to redis.register_function must be a function");
                    }
                    if let Some(r) = lua_ref {
                        luaL_unref(l, LUA_REGISTRYINDEX, r);
                    }
                    lua_ref = Some(luaL_ref(l, LUA_REGISTRYINDEX));
                    continue; // luaL_ref popped the value
                }
                b"flags" => {
                    if lua_type(l, -1) != LUA_TTABLE {
                        return fail(l, lua_ref, "flags argument to redis.register_function must be a table representing function flags");
                    }
                    match read_flags(l) {
                        Some(f) => flags = f,
                        None => return fail(l, lua_ref, "unknown flag given"),
                    }
                }
                _ => return fail(l, lua_ref, "unknown argument given to redis.register_function"),
            }
            lua_pop(l, 1);
        }
        let Some(name) = name else {
            return fail(l, lua_ref, "redis.register_function must get a function name argument");
        };
        let Some(lua_ref) = lua_ref else {
            return Err("redis.register_function must get a callback argument");
        };
        Ok(FnDef { name, desc, flags, lua_ref })
    }
}

/// Function flags from the array on the stack top; `None` on an unknown flag.
unsafe fn read_flags(l: *mut lua_State) -> Option<u64> {
    unsafe {
        let mut flags = 0;
        let mut j = 1;
        loop {
            lua_rawgeti(l, -1, j);
            j += 1;
            if lua_type(l, -1) == LUA_TNIL {
                lua_pop(l, 1);
                return Some(flags);
            }
            let flag = if lua_isstring(l, -1) != 0 { to_bytes(l, -1).map(|b| b.to_ascii_lowercase()) } else { None };
            lua_pop(l, 1);
            flags |= super::script_flag(&flag?)?;
        }
    }
}

// ---- busy scripts ----

/// The instruction-count hook: stop on SCRIPT KILL / FUNCTION KILL or a FUNCTION LOAD timeout.
pub(crate) unsafe extern "C" fn hook_check(l: *mut lua_State) -> c_int {
    unsafe {
        let stop = catch_unwind(AssertUnwindSafe(|| {
            if let Some(run) = run_ctx(l) {
                return run.interrupted().then_some(&b"Script killed by user with SCRIPT KILL..."[..]);
            }
            if let Some(load) = load_ctx(l) {
                return load.timed_out().then_some(&b"FUNCTION LOAD timeout"[..]);
            }
            None
        }))
        .unwrap_or(None);
        match stop {
            Some(msg) => {
                push_error(l, msg);
                1
            }
            None => 0,
        }
    }
}
