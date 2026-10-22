//! Raw bindings to a vendored Lua 5.4 (`lua/`, see its COPYRIGHT) for roll nosql's
//! ScyllaDB-compatible Lua UDFs, plus the glue in `glue.c`: bounded states
//! ([`nl_newstate`] with an allocation limit and a deadline hook that stops a call where `pcall`
//! can't intercept it), the libraries a UDF sees ([`nl_openlibs`]), ScyllaDB's `Scylla.decimal`
//! userdata, and a trampoline for Rust callbacks ([`nl_pushfn`]).
//!
//! Only the core, base, coroutine, string and table libraries are vendored. Redis' Lua 5.1 is a
//! separate crate (`redis-lua`).
//!
//! Lua raises errors with `longjmp`, which must never cross Rust frames: Rust callbacks use
//! non-raising API calls only and return [`NL_RAISE`] / [`NL_DECIMAL`] for the trampoline to raise
//! or allocate. Outside a protected call (disarmed states) allocations don't fail on purpose and
//! a real out-of-memory aborts.
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
pub type lua_Integer = i64;
pub type lua_Unsigned = u64;
pub type lua_Writer =
    unsafe extern "C" fn(l: *mut lua_State, p: *const c_void, sz: usize, ud: *mut c_void) -> c_int;
/// A Rust callback: the number of results it pushed, or [`NL_RAISE`] / [`NL_DECIMAL`] with `out`
/// set.
pub type nl_fn = unsafe extern "C" fn(l: *mut lua_State, out: *mut nl_slice) -> c_int;

/// Raise `out` (a message) as a string error.
pub const NL_RAISE: c_int = -1;
/// Push `out` (a decimal's CQL encoding) as a `Scylla.decimal` and return it.
pub const NL_DECIMAL: c_int = -2;

pub const LUAI_MAXSTACK: c_int = 1_000_000;
pub const LUA_REGISTRYINDEX: c_int = -LUAI_MAXSTACK - 1000;
pub const LUA_MULTRET: c_int = -1;

pub const LUA_OK: c_int = 0;
pub const LUA_YIELD: c_int = 1;
pub const LUA_ERRRUN: c_int = 2;
pub const LUA_ERRSYNTAX: c_int = 3;
pub const LUA_ERRMEM: c_int = 4;
pub const LUA_ERRERR: c_int = 5;

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

/// Limits of a bounded state (see `glue.c`): while `armed`, allocations growing the state past
/// `limit` bytes fail, and past `deadline_ns` (or once `expired` is set) the call is stopped.
#[repr(C)]
#[derive(Debug)]
pub struct nl_bounds {
    pub used: usize,
    pub limit: usize,
    pub armed: c_int,
    pub expired: c_int,
    pub deadline_ns: i64,
    pub host: *mut c_void,
}

impl Default for nl_bounds {
    fn default() -> Self {
        nl_bounds {
            used: 0,
            limit: 0,
            armed: 0,
            expired: 0,
            deadline_ns: 0,
            host: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct nl_slice {
    pub ptr: *const c_char,
    pub len: usize,
}

/// Lua's API under the names `build.rs` gives it (`nosql_lua_*`: rolld also links Redis's Lua
/// 5.1, which exports the same names).
macro_rules! lua_api {
    ($( pub fn $name:ident ( $($arg:ident : $ty:ty),* $(,)? ) $(-> $ret:ty)? ; )*) => {
        unsafe extern "C" {
            $( #[link_name = concat!("nosql_", stringify!($name))] pub fn $name($($arg: $ty),*) $(-> $ret)?; )*
        }
    };
}

lua_api! {
    pub fn lua_close(l: *mut lua_State);
    pub fn lua_newthread(l: *mut lua_State) -> *mut lua_State;
    pub fn lua_resume(
        l: *mut lua_State,
        from: *mut lua_State,
        narg: c_int,
        nres: *mut c_int,
    ) -> c_int;

    pub fn lua_gettop(l: *mut lua_State) -> c_int;
    pub fn lua_settop(l: *mut lua_State, idx: c_int);
    pub fn lua_pushvalue(l: *mut lua_State, idx: c_int);
    pub fn lua_rotate(l: *mut lua_State, idx: c_int, n: c_int);
    pub fn lua_checkstack(l: *mut lua_State, n: c_int) -> c_int;
    pub fn lua_xmove(from: *mut lua_State, to: *mut lua_State, n: c_int);

    pub fn lua_type(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_isinteger(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_tonumberx(l: *mut lua_State, idx: c_int, isnum: *mut c_int) -> lua_Number;
    pub fn lua_tointegerx(l: *mut lua_State, idx: c_int, isnum: *mut c_int) -> lua_Integer;
    pub fn lua_toboolean(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_tolstring(l: *mut lua_State, idx: c_int, len: *mut usize) -> *const c_char;
    pub fn lua_rawlen(l: *mut lua_State, idx: c_int) -> lua_Unsigned;
    pub fn lua_touserdata(l: *mut lua_State, idx: c_int) -> *mut c_void;
    pub fn lua_rawequal(l: *mut lua_State, a: c_int, b: c_int) -> c_int;
    pub fn lua_stringtonumber(l: *mut lua_State, s: *const c_char) -> usize;

    pub fn lua_pushnil(l: *mut lua_State);
    pub fn lua_pushnumber(l: *mut lua_State, n: lua_Number);
    pub fn lua_pushinteger(l: *mut lua_State, n: lua_Integer);
    pub fn lua_pushlstring(l: *mut lua_State, s: *const c_char, len: usize) -> *const c_char;
    pub fn lua_pushboolean(l: *mut lua_State, b: c_int);

    pub fn lua_createtable(l: *mut lua_State, narr: c_int, nrec: c_int);
    pub fn lua_rawget(l: *mut lua_State, idx: c_int) -> c_int;
    pub fn lua_rawgeti(l: *mut lua_State, idx: c_int, n: lua_Integer) -> c_int;
    pub fn lua_rawset(l: *mut lua_State, idx: c_int);
    pub fn lua_rawseti(l: *mut lua_State, idx: c_int, n: lua_Integer);
    pub fn lua_setfield(l: *mut lua_State, idx: c_int, k: *const c_char);
    pub fn lua_next(l: *mut lua_State, idx: c_int) -> c_int;

    pub fn luaL_loadbufferx(
        l: *mut lua_State,
        buf: *const c_char,
        sz: usize,
        name: *const c_char,
        mode: *const c_char,
    ) -> c_int;
    pub fn lua_dump(
        l: *mut lua_State,
        writer: lua_Writer,
        data: *mut c_void,
        strip: c_int,
    ) -> c_int;
}

unsafe extern "C" {
    /// A state whose allocations and running time are bounded by `b` (kept alive, at a fixed
    /// address, until `lua_close`); its panic handler aborts.
    pub fn nl_newstate(b: *mut nl_bounds) -> *mut lua_State;
    /// The bounds `l` was created with (for callbacks). Doesn't raise.
    pub fn nl_bounds_of(l: *mut lua_State) -> *mut nl_bounds;
    /// `CLOCK_MONOTONIC` in nanoseconds (for [`nl_bounds::deadline_ns`]).
    pub fn nl_monotonic_ns() -> i64;
    /// Installs the deadline hook (threads created from `l` inherit it).
    pub fn nl_sethook(l: *mut lua_State);
    /// Opens ScyllaDB's UDF libraries: base, coroutine, string, table; dofile/loadfile see no
    /// files, print/warn print nothing, load takes text chunks only.
    pub fn nl_openlibs(l: *mut lua_State);
    /// Pushes a Rust callback as a C closure.
    pub fn nl_pushfn(l: *mut lua_State, f: nl_fn);
    /// Creates the `Scylla.decimal` metatable with the host's `__add` / `__sub`.
    pub fn nl_decimal_mt(l: *mut lua_State, add: nl_fn, sub: nl_fn);
    /// Pushes a decimal (its CQL encoding) as a `Scylla.decimal`.
    pub fn nl_pushdecimal(l: *mut lua_State, bytes: *const c_char, len: usize);
    /// The CQL encoding of the `Scylla.decimal` at `idx`, or null. Doesn't raise.
    pub fn nl_todecimal(l: *mut lua_State, idx: c_int, len: *mut usize) -> *const c_char;
    /// `lua_Number2str` (`%.14g`, `.0` appended to integral values) into `buf`; the length.
    pub fn nl_fmt_number(d: c_double, buf: *mut c_char, n: usize) -> c_int;
}

#[inline]
pub unsafe fn lua_pop(l: *mut lua_State, n: c_int) {
    unsafe { lua_settop(l, -n - 1) }
}

#[inline]
pub unsafe fn lua_insert(l: *mut lua_State, idx: c_int) {
    unsafe { lua_rotate(l, idx, 1) }
}

#[inline]
pub unsafe fn lua_remove(l: *mut lua_State, idx: c_int) {
    unsafe {
        lua_rotate(l, idx, -1);
        lua_pop(l, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_runs_a_chunk() {
        let mut b = Box::new(nl_bounds::default());
        unsafe {
            let l = nl_newstate(&mut *b);
            assert!(!l.is_null());
            nl_openlibs(l);
            let src = b"return _VERSION .. tostring(math)";
            assert_eq!(
                luaL_loadbufferx(
                    l,
                    src.as_ptr().cast(),
                    src.len(),
                    c"=t".as_ptr(),
                    c"t".as_ptr()
                ),
                LUA_OK
            );
            let co = lua_newthread(l);
            lua_rotate(l, -2, 1);
            lua_xmove(l, co, 1);
            let mut n = 0;
            assert_eq!(lua_resume(co, l, 0, &mut n), LUA_OK);
            let mut len = 0;
            let p = lua_tolstring(co, -1, &mut len);
            assert_eq!(
                std::slice::from_raw_parts(p.cast::<u8>(), len),
                b"Lua 5.4nil"
            );
            lua_close(l);
        }
    }
}
