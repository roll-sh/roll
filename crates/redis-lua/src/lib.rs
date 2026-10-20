//! Raw bindings to the Lua 5.1 that Redis embeds: Valkey's `deps/lua` (read-only tables, cjson,
//! cmsgpack, struct, bit) with Redis' string hash, so table iteration order matches Redis.
//!
//! Lua raises errors with `longjmp`, which must never cross Rust frames. Rust callbacks are
//! pushed with [`roll_pushfn`] and signal errors by returning [`RAISE`] (error object on the
//! stack top) or [`RAISE_WHERE`] (message to prefix with the caller's position); the C
//! trampoline raises. Allocation failures abort instead of raising.
//!
//! # Safety
//!
//! Every function here, including the small inline helpers, has the Lua C API's contract: a
//! valid state, valid stack indices, enough stack space.

#![allow(non_camel_case_types, clippy::missing_safety_doc)]

use std::ffi::{c_char, c_double, c_int, c_void};

#[repr(C)]
pub struct lua_State {
    _private: [u8; 0],
}

pub type lua_Number = c_double;
pub type lua_Integer = isize;
pub type lua_CFunction = unsafe extern "C" fn(*mut lua_State) -> c_int;
/// A Rust callback: returns the number of results, or [`RAISE`] / [`RAISE_WHERE`].
pub type roll_fn = unsafe extern "C" fn(*mut lua_State) -> c_int;
/// The debugger's callback for each line (and every 100k instructions, `count` set) of the
/// current frame: non-zero raises the error object it pushed.
pub type roll_ldb_fn = unsafe extern "C" fn(l: *mut lua_State, count: c_int, line: c_int, src: *const c_char) -> c_int;

pub const RAISE: c_int = -1;
pub const RAISE_WHERE: c_int = -2;

pub const LUA_REGISTRYINDEX: c_int = -10000;
pub const LUA_ENVIRONINDEX: c_int = -10001;
pub const LUA_GLOBALSINDEX: c_int = -10002;
pub const LUA_MULTRET: c_int = -1;

pub const LUA_TNONE: c_int = -1;
pub const LUA_TNIL: c_int = 0;
pub const LUA_TBOOLEAN: c_int = 1;
pub const LUA_TLIGHTUSERDATA: c_int = 2;
pub const LUA_TNUMBER: c_int = 3;
pub const LUA_TSTRING: c_int = 4;
pub const LUA_TTABLE: c_int = 5;
pub const LUA_TFUNCTION: c_int = 6;
pub const LUA_TUSERDATA: c_int = 7;
pub const LUA_TTHREAD: c_int = 8;

pub const LUA_GCSTOP: c_int = 0;
pub const LUA_GCRESTART: c_int = 1;
pub const LUA_GCCOLLECT: c_int = 2;
pub const LUA_GCCOUNT: c_int = 3;
pub const LUA_GCCOUNTB: c_int = 4;
pub const LUA_GCSTEP: c_int = 5;

pub const LUA_NOREF: c_int = -2;
pub const LUA_REFNIL: c_int = -1;

unsafe extern "C" {
    pub fn lua_close(l: *mut lua_State);
    pub fn lua_gettop(l: *mut lua_State) -> c_int;
    pub fn lua_settop(l: *mut lua_State, idx: c_int);
    pub fn lua_pushvalue(l: *mut lua_State, idx: c_int);
    pub fn lua_remove(l: *mut lua_State, idx: c_int);
    pub fn lua_insert(l: *mut lua_State, idx: c_int);
    pub fn lua_replace(l: *mut lua_State, idx: c_int);
    pub fn lua_checkstack(l: *mut lua_State, sz: c_int) -> c_int;

    pub fn lua_isnumber(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_isstring(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_iscfunction(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_type(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_rawequal(l: *mut lua_State, a: c_int, b: c_int) -> c_int;

    pub fn lua_tonumber(l: *mut lua_State, idx: c_int) -> lua_Number;
    pub fn lua_tointeger(l: *mut lua_State, idx: c_int) -> lua_Integer;
    pub fn lua_toboolean(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_tolstring(l: *mut lua_State, idx: c_int, len: *mut usize) -> *const c_char;
    pub fn lua_objlen(l: *mut lua_State, idx: c_int) -> usize;
    pub fn lua_touserdata(l: *mut lua_State, idx: c_int) -> *mut c_void;
    pub fn lua_topointer(l: *mut lua_State, idx: c_int) -> *const c_void;

    pub fn lua_pushnil(l: *mut lua_State);
    pub fn lua_pushnumber(l: *mut lua_State, n: lua_Number);
    pub fn lua_pushinteger(l: *mut lua_State, n: lua_Integer);
    pub fn lua_pushlstring(l: *mut lua_State, s: *const c_char, len: usize);
    pub fn lua_pushstring(l: *mut lua_State, s: *const c_char);
    pub fn lua_pushcclosure(l: *mut lua_State, f: lua_CFunction, n: c_int);
    pub fn lua_pushboolean(l: *mut lua_State, b: c_int);
    pub fn lua_pushlightuserdata(l: *mut lua_State, p: *mut c_void);

    pub fn lua_gettable(l: *mut lua_State, idx: c_int);
    pub fn lua_getfield(l: *mut lua_State, idx: c_int, k: *const c_char);
    pub fn lua_rawget(l: *mut lua_State, idx: c_int);
    pub fn lua_rawgeti(l: *mut lua_State, idx: c_int, n: c_int);
    pub fn lua_createtable(l: *mut lua_State, narr: c_int, nrec: c_int);
    pub fn lua_getmetatable(l: *mut lua_State, idx: c_int) -> c_int;

    pub fn lua_settable(l: *mut lua_State, idx: c_int);
    pub fn lua_setfield(l: *mut lua_State, idx: c_int, k: *const c_char);
    pub fn lua_rawset(l: *mut lua_State, idx: c_int);
    pub fn lua_rawseti(l: *mut lua_State, idx: c_int, n: c_int);
    pub fn lua_setmetatable(l: *mut lua_State, idx: c_int) -> c_int;

    pub fn lua_call(l: *mut lua_State, nargs: c_int, nresults: c_int);
    pub fn lua_pcall(l: *mut lua_State, nargs: c_int, nresults: c_int, errfunc: c_int) -> c_int;
    pub fn lua_next(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_gc(l: *mut lua_State, what: c_int, data: c_int) -> c_int;

    pub fn lua_enablereadonlytable(l: *mut lua_State, idx: c_int, enabled: c_int);
    pub fn lua_isreadonlytable(l: *mut lua_State, idx: c_int) -> c_int;

    pub fn luaL_loadbuffer(l: *mut lua_State, buf: *const c_char, sz: usize, name: *const c_char) -> c_int;
    pub fn luaL_ref(l: *mut lua_State, t: c_int) -> c_int;
    pub fn luaL_unref(l: *mut lua_State, t: c_int, r: c_int);

    // glue.c
    /// A new state whose allocator aborts on failure.
    pub fn roll_newstate() -> *mut lua_State;
    /// Push a Rust callback as a Lua C function.
    pub fn roll_pushfn(l: *mut lua_State, f: roll_fn);
    /// Open base, table, string, math, debug, os (sandboxed), cjson, struct, cmsgpack and bit.
    pub fn roll_open_libs(l: *mut lua_State);
    /// Only allow-listed globals can be created in the table on the stack top.
    pub fn roll_set_allowlist(l: *mut lua_State, deprecated_api: c_int);
    /// Reading a missing field of the table on the stack top raises "nonexistent global".
    pub fn roll_set_error_metatable(l: *mut lua_State);
    /// Make the table on the stack top, and everything reachable from it, read-only.
    pub fn roll_protect(l: *mut lua_State);
    pub fn roll_protect_basic_types(l: *mut lua_State);
    /// Push Redis' `pcall` (turns `{err = msg}` error tables into `msg`).
    pub fn roll_push_pcall(l: *mut lua_State);
    /// Install math.random / math.randomseed (drand48) into the table on the stack top.
    pub fn roll_set_random(l: *mut lua_State);
    /// Instruction-count hook: `check` returns non-zero (with an error object pushed) to stop
    /// the script. `None` removes the hook.
    pub fn roll_sethook(l: *mut lua_State, check: Option<roll_fn>);
    /// The debugger hook (lines and every 100k instructions); [`roll_sethook`] with `None`
    /// removes it.
    pub fn roll_setldbhook(l: *mut lua_State, line: roll_ldb_fn);
    /// Stack frame `level`: 0 when there is none, else 1 with its current line, short source and
    /// name (null when unknown), valid until the next call.
    pub fn roll_frame(l: *mut lua_State, level: c_int, line: *mut c_int, src: *mut *const c_char, name: *mut *const c_char) -> c_int;
    /// Local `n` (from 1) of frame `level`: its value pushed and its name returned, or null.
    pub fn roll_getlocal(l: *mut lua_State, level: c_int, n: c_int) -> *const c_char;
}

#[inline]
pub unsafe fn lua_pop(l: *mut lua_State, n: c_int) {
    unsafe { lua_settop(l, -n - 1) }
}

#[inline]
pub unsafe fn lua_newtable(l: *mut lua_State) {
    unsafe { lua_createtable(l, 0, 0) }
}

#[inline]
pub const fn lua_upvalueindex(i: c_int) -> c_int {
    LUA_GLOBALSINDEX - i
}

#[inline]
pub unsafe fn lua_pushcfunction(l: *mut lua_State, f: lua_CFunction) {
    unsafe { lua_pushcclosure(l, f, 0) }
}

#[inline]
pub unsafe fn lua_getglobal(l: *mut lua_State, name: *const c_char) {
    unsafe { lua_getfield(l, LUA_GLOBALSINDEX, name) }
}

#[inline]
pub unsafe fn lua_setglobal(l: *mut lua_State, name: *const c_char) {
    unsafe { lua_setfield(l, LUA_GLOBALSINDEX, name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_lua_with_redis_libraries() {
        unsafe {
            let l = roll_newstate();
            roll_open_libs(l);
            let code = b"return cjson.encode({1,2,3}) .. bit.tohex(255) .. string.format('%d', 7)";
            assert_eq!(luaL_loadbuffer(l, code.as_ptr().cast(), code.len(), c"@test".as_ptr()), 0);
            assert_eq!(lua_pcall(l, 0, 1, 0), 0);
            let mut len = 0;
            let s = lua_tolstring(l, -1, &mut len);
            assert_eq!(std::slice::from_raw_parts(s.cast::<u8>(), len), b"[1,2,3]000000ff7");
            lua_close(l);
        }
    }
}
