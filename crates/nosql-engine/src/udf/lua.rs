//! ScyllaDB's Lua UDFs, on a vendored Lua 5.4 (`nosql-lua`), as `scylladb/scylla:2026.3.3` runs
//! them (every rule below was probed on the reference: `docs/nosql/reference/capture/udf_cases.py`).
//!
//! **Compiling.** The body becomes the chunk `local <args> = ...\n<body>` (just the body without
//! arguments) named `<internal>`, so compile errors read `[string "<internal>"]:<line>: …`;
//! an empty body fails as ScyllaDB's reader does (`unexpected symbol near '<\255>'`). The chunk
//! is kept as stripped bytecode, so runtime errors carry no position (`?:-1: …`).
//!
//! **Running.** Every call gets a fresh state (`nl_openlibs`: base, coroutine, string, table;
//! no files, no output, text-only `load`) and runs on its main thread through `lua_resume`, as
//! ScyllaDB does (`coroutine.running()` is the main thread and yieldable). Limits are ScyllaDB's
//! defaults: [`ALLOCATION_LIMIT`] on what the call allocates (`not enough memory`) and
//! [`TIME_LIMIT_MS`] of running time, enforced by a count hook that yields the call once the
//! deadline passes (`pcall` can't intercept a yield; a yield from a non-yieldable spot raises
//! Lua's own `attempt to yield from outside a coroutine`, as on the reference). The result must
//! be exactly one value (`N values returned, expected 1`).
//!
//! **Values.** Integers of every width, `counter`, `date` (days since the epoch + 2^31), `time`
//! (nanoseconds since midnight) and `timestamp` (milliseconds) are Lua integers; `float` and
//! `double` floats; `varint` and `decimal` a `Scylla.decimal` userdata with only `__add` /
//! `__sub` (a float operand makes the result a float; a conversion error inside them fails the
//! whole call, uncatchable as on the reference); text, blob, inet and uuids strings; booleans;
//! lists, tuples and vectors sequences (null components are holes); sets `{[e] = true}`; maps
//! `{[k] = v}`; UDTs tables by field name; durations `{months, days, nanoseconds}`; empty
//! non-frozen collections arrive as nil. Results convert back with ScyllaDB's coercions and
//! messages ([`pull`]): integers from integral floats (`-2^63 <= f <= 2^63`, 2^63 wrapping to
//! `i64::MIN` as x86 does) and numeric strings (exact past 64 bits), wrapping to the type's
//! width; floats as text in `{fmt}`'s shortest form; dates, times and timestamps also from
//! literals and date tables (`{year, month, day[, hour, min, sec]}`, normalized); sets and maps
//! sorted but not deduplicated.
//!
//! Lua raises errors with `longjmp`: the host only calls non-raising API functions outside a
//! protected call, and its callbacks report through `nosql-lua`'s trampoline. Allocation limits
//! apply only while the state is armed (inside `lua_resume`).

use std::ffi::{CString, c_char, c_int, c_void};

use bytes::Bytes;
use nosql_lua as lua;
use nosql_types::{CqlType, Decimal, Dialect, Value};
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

/// `user_defined_function_allocation_limit_bytes` (ScyllaDB's default).
pub const ALLOCATION_LIMIT: usize = 1 << 20;
/// `user_defined_function_time_limit_ms` (ScyllaDB's default).
pub const TIME_LIMIT_MS: i64 = 10;

const CHUNK: &std::ffi::CStr = c"<internal>";

/// Why a call failed.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// A Lua error: `lua execution failed: <message>` (InvalidRequest).
    Runtime(String),
    /// Ran past [`TIME_LIMIT_MS`]: `lua execution timeout: <ms>ms elapsed` (InvalidRequest).
    Timeout(f64),
    /// InvalidRequest with this message (conversions, the result count).
    Invalid(String),
    /// ServerError with this message (literal parsing in conversions).
    Server(String),
    /// The result converted to bytes that aren't a valid value of the return type (ScyllaDB's
    /// `runtime error: Return of function … is not a valid value …`).
    InvalidReturn(Bytes),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Runtime(m) => write!(f, "lua execution failed: {m}"),
            Failure::Timeout(ms) => write!(f, "lua execution timeout: {ms}ms elapsed"),
            Failure::Invalid(m) | Failure::Server(m) => write!(f, "{m}"),
            Failure::InvalidReturn(_) => write!(f, "invalid return value"),
        }
    }
}

const NOT_NUMBER: &str = "value is not a number";
const NOT_INTEGER: &str = "value is not an integer";
const NOT_DECIMAL: &str = "value is not a decimal";
const UNEXPECTED: &str = "unexpected value";
const NOT_TABLE: &str = "value is not a table";

fn invalid(m: &str) -> Failure {
    Failure::Invalid(m.to_string())
}

/// What a callback of the state needs: scratch space for its out-parameter and the first
/// conversion error raised inside a metamethod (which fails the call whatever Lua does next).
#[derive(Default)]
struct Host {
    scratch: Vec<u8>,
    fatal: Option<Failure>,
}

/// One bounded Lua state with ScyllaDB's libraries.
struct Vm {
    l: *mut lua::lua_State,
    bounds: *mut lua::nl_bounds,
    host: *mut Host,
}

impl Vm {
    fn new() -> Option<Vm> {
        let host = Box::into_raw(Box::<Host>::default());
        let bounds = Box::into_raw(Box::new(lua::nl_bounds {
            host: host.cast::<c_void>(),
            ..Default::default()
        }));
        // SAFETY: `bounds` stays valid (and in place) until `Drop` frees it after `lua_close`.
        let l = unsafe { lua::nl_newstate(bounds) };
        if l.is_null() {
            // SAFETY: allocated above, never shared.
            unsafe {
                drop(Box::from_raw(bounds));
                drop(Box::from_raw(host));
            }
            return None;
        }
        // SAFETY: a fresh, disarmed state: these only allocate, which can't fail on purpose.
        unsafe {
            lua::nl_openlibs(l);
            lua::nl_decimal_mt(l, decimal_add, decimal_sub);
            lua::lua_settop(l, 0);
            lua::nl_sethook(l);
        }
        Some(Vm { l, bounds, host })
    }

    fn arm(&self) {
        // SAFETY: `bounds` is valid while the state lives; nothing else runs on it now.
        unsafe {
            let b = &mut *self.bounds;
            b.limit = b.used.saturating_add(ALLOCATION_LIMIT);
            b.deadline_ns = lua::nl_monotonic_ns().saturating_add(TIME_LIMIT_MS * 1_000_000);
            b.expired = 0;
            b.armed = 1;
        }
    }

    fn disarm(&self) {
        // SAFETY: as in `arm`.
        unsafe { (*self.bounds).armed = 0 }
    }

    fn expired(&self) -> bool {
        // SAFETY: as in `arm`.
        unsafe { (*self.bounds).expired != 0 }
    }

    fn fatal(&self) -> Option<Failure> {
        // SAFETY: the host lives as long as the state; callbacks have returned.
        unsafe { (*self.host).fatal.take() }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // SAFETY: the state, its bounds and host were created together in `new`.
        unsafe {
            (*self.bounds).armed = 0;
            lua::lua_close(self.l);
            drop(Box::from_raw(self.bounds));
            drop(Box::from_raw(self.host));
        }
    }
}

/// The message of the error object on the stack top (strings and numbers; ScyllaDB itself
/// crashes on other error objects).
///
/// # Safety
/// A valid, disarmed state with the error object on top.
unsafe fn error_message(l: *mut lua::lua_State) -> String {
    unsafe {
        match lua::lua_type(l, -1) {
            lua::LUA_TSTRING | lua::LUA_TNUMBER => {
                lua::lua_pushvalue(l, -1);
                let s = raw_string(l, -1);
                lua::lua_pop(l, 1);
                String::from_utf8_lossy(&s).into_owned()
            }
            t => format!("(error object is a {} value)", type_name(t)),
        }
    }
}

fn type_name(t: c_int) -> &'static str {
    match t {
        lua::LUA_TNIL => "nil",
        lua::LUA_TBOOLEAN => "boolean",
        lua::LUA_TLIGHTUSERDATA | lua::LUA_TUSERDATA => "userdata",
        lua::LUA_TNUMBER => "number",
        lua::LUA_TSTRING => "string",
        lua::LUA_TTABLE => "table",
        lua::LUA_TFUNCTION => "function",
        lua::LUA_TTHREAD => "thread",
        _ => "no",
    }
}

/// The bytes of the string at `idx` (numbers are converted in place: disarmed states only).
///
/// # Safety
/// A valid state and index.
unsafe fn raw_string(l: *mut lua::lua_State, idx: c_int) -> Vec<u8> {
    let mut len = 0usize;
    // SAFETY: per the contract; the pointer is valid while the value stays on the stack.
    unsafe {
        let p = lua::lua_tolstring(l, idx, &mut len);
        if p.is_null() {
            return Vec::new();
        }
        std::slice::from_raw_parts(p.cast::<u8>(), len).to_vec()
    }
}

unsafe extern "C" fn dump_writer(
    _l: *mut lua::lua_State,
    p: *const c_void,
    sz: usize,
    ud: *mut c_void,
) -> c_int {
    // SAFETY: `ud` is the `Vec` passed to `lua_dump` below; `p` holds `sz` bytes.
    unsafe {
        let out = &mut *ud.cast::<Vec<u8>>();
        out.extend_from_slice(std::slice::from_raw_parts(p.cast::<u8>(), sz));
    }
    0
}

/// Compiles a body to stripped bytecode, or ScyllaDB's compile error (without the
/// `could not compile: ` prefix).
pub fn compile(arg_names: &[String], body: &str) -> Result<Vec<u8>, String> {
    let line = if arg_names.is_empty() { 1 } else { 2 };
    if body.is_empty() {
        return Err(format!(
            "[string \"<internal>\"]:{line}: unexpected symbol near '<\\255>'"
        ));
    }
    let src = if arg_names.is_empty() {
        body.to_string()
    } else {
        format!("local {} = ...\n{body}", arg_names.join(", "))
    };
    let vm = Vm::new().ok_or_else(|| "cannot create a Lua state".to_string())?;
    // SAFETY: a valid, disarmed state; loading and dumping report errors without raising.
    unsafe {
        let rc = lua::luaL_loadbufferx(
            vm.l,
            src.as_ptr().cast::<c_char>(),
            src.len(),
            CHUNK.as_ptr(),
            c"t".as_ptr(),
        );
        if rc != lua::LUA_OK {
            return Err(error_message(vm.l));
        }
        let mut out: Vec<u8> = Vec::new();
        lua::lua_dump(
            vm.l,
            dump_writer,
            (&mut out as *mut Vec<u8>).cast::<c_void>(),
            1,
        );
        Ok(out)
    }
}

/// Calls compiled bytecode with typed, serialised arguments and converts its result.
pub fn call(
    code: &[u8],
    args: &[(&CqlType, Option<&[u8]>)],
    ret: &CqlType,
) -> Result<Option<Bytes>, Failure> {
    let vm = Vm::new().ok_or_else(|| invalid("cannot create a Lua state"))?;
    let l = vm.l;
    // SAFETY: a valid state. Pushing arguments and reading the result happen disarmed (nothing
    // fails on purpose); running happens inside `lua_resume`.
    unsafe {
        if lua::luaL_loadbufferx(
            l,
            code.as_ptr().cast::<c_char>(),
            code.len(),
            CHUNK.as_ptr(),
            c"b".as_ptr(),
        ) != lua::LUA_OK
        {
            return Err(Failure::Runtime(error_message(l)));
        }
        if lua::lua_checkstack(l, args.len() as c_int + 8) == 0 {
            return Err(invalid("too many arguments"));
        }
        for (ty, v) in args {
            match v {
                None => lua::lua_pushnil(l),
                Some(b) => {
                    let value = nosql_types::decode(ty, b).map_err(|e| invalid(&e.message))?;
                    push(l, ty, &value)?;
                }
            }
        }
        let start = std::time::Instant::now();
        let mut nargs = args.len() as c_int;
        vm.arm();
        let (rc, nres) = loop {
            let mut nres = 0;
            let rc = lua::lua_resume(l, std::ptr::null_mut(), nargs, &mut nres);
            if rc == lua::LUA_YIELD && !vm.expired() {
                lua::lua_pop(l, nres);
                nargs = 0;
                continue;
            }
            break (rc, nres);
        };
        vm.disarm();
        if let Some(f) = vm.fatal() {
            return Err(f);
        }
        if vm.expired() {
            let ms = start.elapsed().as_nanos() as f64 / 1e6;
            return Err(Failure::Timeout(ms));
        }
        if rc == lua::LUA_ERRMEM {
            return Err(Failure::Runtime("not enough memory".into()));
        }
        if rc != lua::LUA_OK {
            return Err(Failure::Runtime(error_message(l)));
        }
        if nres != 1 {
            return Err(Failure::Invalid(format!(
                "{nres} values returned, expected 1"
            )));
        }
        let top = lua::lua_gettop(l);
        let v = pull(l, top, ret)?;
        if matches!(v, Value::Null) {
            return Ok(None);
        }
        let b = nosql_types::encode(&v, ret);
        if matches!(ret, CqlType::Duration)
            && nosql_types::Duration::validate(&b, Dialect::Scylla).is_err()
        {
            return Err(Failure::InvalidReturn(b));
        }
        Ok(Some(b))
    }
}

// ---- CQL → Lua -------------------------------------------------------------------------------

/// Pushes `v` (of type `ty`).
///
/// # Safety
/// A valid, disarmed state.
unsafe fn push(l: *mut lua::lua_State, ty: &CqlType, v: &Value) -> Result<(), Failure> {
    // SAFETY: per the contract; every call below only allocates, which can't fail on purpose.
    unsafe {
        if lua::lua_checkstack(l, 4) == 0 {
            return Err(invalid("value is nested too deeply"));
        }
        let push_str = |s: &[u8]| {
            lua::lua_pushlstring(l, s.as_ptr().cast::<c_char>(), s.len());
        };
        let empty_multicell = |n: usize| {
            n == 0
                && matches!(
                    ty,
                    CqlType::List { frozen: false, .. }
                        | CqlType::Set { frozen: false, .. }
                        | CqlType::Map { frozen: false, .. }
                )
        };
        match v {
            Value::Null | Value::Empty => lua::lua_pushnil(l),
            Value::Ascii(s) | Value::Text(s) => push_str(s.as_bytes()),
            Value::Blob(b) | Value::Custom(b) => push_str(b),
            Value::Boolean(b) => lua::lua_pushboolean(l, *b as c_int),
            Value::Tinyint(n) => lua::lua_pushinteger(l, *n as i64),
            Value::Smallint(n) => lua::lua_pushinteger(l, *n as i64),
            Value::Int(n) => lua::lua_pushinteger(l, *n as i64),
            Value::Bigint(n) | Value::Counter(n) | Value::Timestamp(n) | Value::Time(n) => {
                lua::lua_pushinteger(l, *n)
            }
            Value::Date(n) => lua::lua_pushinteger(l, *n as i64),
            Value::Float(f) => lua::lua_pushnumber(l, *f as f64),
            Value::Double(f) => lua::lua_pushnumber(l, *f),
            Value::Varint(n) => push_decimal(l, &Decimal::new(n.clone(), 0)),
            Value::Decimal(d) => push_decimal(l, d),
            Value::Uuid(u) | Value::Timeuuid(u) => push_str(format_uuid(u).as_bytes()),
            Value::Inet(ip) => push_str(ip.to_string().as_bytes()),
            Value::Duration(du) => {
                lua::lua_createtable(l, 0, 3);
                for (k, n) in [
                    (c"months", du.months as i64),
                    (c"days", du.days as i64),
                    (c"nanoseconds", du.nanos),
                ] {
                    lua::lua_pushinteger(l, n);
                    lua::lua_setfield(l, -2, k.as_ptr());
                }
            }
            Value::List(items) | Value::Set(items) if empty_multicell(items.len()) => {
                lua::lua_pushnil(l)
            }
            Value::Map(entries) if empty_multicell(entries.len()) => lua::lua_pushnil(l),
            Value::List(items) | Value::Vector(items) | Value::Tuple(items) => {
                lua::lua_createtable(l, items.len().min(1 << 20) as c_int, 0);
                for (i, item) in items.iter().enumerate() {
                    if matches!(item, Value::Null) {
                        continue;
                    }
                    let ety = element_type(ty, i);
                    push(l, &ety, item)?;
                    lua::lua_rawseti(l, -2, (i + 1) as i64);
                }
            }
            Value::Set(items) => {
                lua::lua_createtable(l, 0, items.len().min(1 << 20) as c_int);
                let ety = element_type(ty, 0);
                for item in items {
                    push(l, &ety, item)?;
                    lua::lua_pushboolean(l, 1);
                    lua::lua_rawset(l, -3);
                }
            }
            Value::Map(entries) => {
                lua::lua_createtable(l, 0, entries.len().min(1 << 20) as c_int);
                let (kt, vt) = match ty {
                    CqlType::Map { key, value, .. } => ((**key).clone(), (**value).clone()),
                    _ => (CqlType::Blob, CqlType::Blob),
                };
                for (k, val) in entries {
                    push(l, &kt, k)?;
                    push(l, &vt, val)?;
                    lua::lua_rawset(l, -3);
                }
            }
            Value::Udt(fields) => {
                lua::lua_createtable(l, 0, fields.len().min(1 << 20) as c_int);
                if let CqlType::Udt(def, _) = ty {
                    for ((name, fty), fv) in def.fields.iter().zip(fields) {
                        if matches!(fv, Value::Null) {
                            continue;
                        }
                        push_str(name.as_bytes());
                        push(l, fty, fv)?;
                        lua::lua_rawset(l, -3);
                    }
                }
            }
        }
        Ok(())
    }
}

/// # Safety
/// A valid state with the decimal metatable.
unsafe fn push_decimal(l: *mut lua::lua_State, d: &Decimal) {
    let b = d.to_bytes();
    // SAFETY: per the contract (allocates; can't fail on purpose while disarmed).
    unsafe { lua::nl_pushdecimal(l, b.as_ptr().cast::<c_char>(), b.len()) }
}

fn element_type(ty: &CqlType, i: usize) -> CqlType {
    match ty {
        CqlType::List { elem, .. } | CqlType::Set { elem, .. } | CqlType::Vector { elem, .. } => {
            (**elem).clone()
        }
        CqlType::Tuple(ts) => ts.get(i).cloned().unwrap_or(CqlType::Blob),
        _ => CqlType::Blob,
    }
}

fn format_uuid(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

// ---- Lua values ------------------------------------------------------------------------------

/// A Lua value as the conversions see it.
enum Lv {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Dec(Decimal),
    Table,
    Other(c_int),
}

/// Reads the value at `idx` with non-raising accessors only (numbers are never converted).
///
/// # Safety
/// A valid state and index.
unsafe fn classify(l: *mut lua::lua_State, idx: c_int) -> Lv {
    // SAFETY: per the contract.
    unsafe {
        match lua::lua_type(l, idx) {
            lua::LUA_TNIL | lua::LUA_TNONE => Lv::Nil,
            lua::LUA_TBOOLEAN => Lv::Bool(lua::lua_toboolean(l, idx) != 0),
            lua::LUA_TNUMBER => {
                if lua::lua_isinteger(l, idx) != 0 {
                    Lv::Int(lua::lua_tointegerx(l, idx, std::ptr::null_mut()))
                } else {
                    Lv::Float(lua::lua_tonumberx(l, idx, std::ptr::null_mut()))
                }
            }
            lua::LUA_TSTRING => Lv::Str(raw_string(l, idx)),
            lua::LUA_TTABLE => Lv::Table,
            lua::LUA_TUSERDATA => {
                let mut len = 0usize;
                let p = lua::nl_todecimal(l, idx, &mut len);
                if p.is_null() {
                    return Lv::Other(lua::LUA_TUSERDATA);
                }
                let b = std::slice::from_raw_parts(p.cast::<u8>(), len);
                match Decimal::from_bytes(b) {
                    Some(d) => Lv::Dec(d),
                    None => Lv::Other(lua::LUA_TUSERDATA),
                }
            }
            t => Lv::Other(t),
        }
    }
}

/// A Lua number.
#[derive(Clone, Copy)]
enum Num {
    Int(i64),
    Float(f64),
}

/// The number a string converts to as Lua converts it (`lua_stringtonumber`).
///
/// # Safety
/// A valid state with a free stack slot.
unsafe fn str_number(l: *mut lua::lua_State, s: &[u8]) -> Option<Num> {
    let c = CString::new(s).ok()?;
    // SAFETY: per the contract; the conversion pushes a number (no allocation) on success.
    unsafe {
        if lua::lua_checkstack(l, 1) == 0 || lua::lua_stringtonumber(l, c.as_ptr()) == 0 {
            return None;
        }
        let n = if lua::lua_isinteger(l, -1) != 0 {
            Num::Int(lua::lua_tointegerx(l, -1, std::ptr::null_mut()))
        } else {
            Num::Float(lua::lua_tonumberx(l, -1, std::ptr::null_mut()))
        };
        lua::lua_pop(l, 1);
        Some(n)
    }
}

/// A float as an `i64` when it is integral and `-2^63 <= f <= 2^63` (ScyllaDB's bound; 2^63
/// itself converts as x86 does, to `i64::MIN`).
fn float_integer(f: f64) -> Option<i64> {
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if f.is_nan() || f.trunc() != f || !(-TWO_63..=TWO_63).contains(&f) {
        return None;
    }
    Some(if f == TWO_63 { i64::MIN } else { f as i64 })
}

/// The integer value of a decimal, if it is integral.
fn decimal_integer(d: &Decimal) -> Option<BigInt> {
    if d.scale <= 0 {
        return Some(&d.unscaled * BigInt::from(10u8).pow(d.scale.unsigned_abs()));
    }
    let p = BigInt::from(10u8).pow(d.scale as u32);
    let (q, r) = (&d.unscaled / &p, &d.unscaled % &p);
    r.is_zero().then_some(q)
}

/// A number-like value (number, numeric string, decimal) as an exact integer: `Ok(None)` when
/// it isn't integral, `Err` when it isn't number-like.
///
/// # Safety
/// A valid state.
unsafe fn integral(l: *mut lua::lua_State, v: &Lv) -> Result<Option<BigInt>, Failure> {
    // SAFETY: per the contract.
    unsafe {
        Ok(match v {
            Lv::Int(i) => Some(BigInt::from(*i)),
            Lv::Float(f) => float_integer(*f).map(BigInt::from),
            Lv::Str(s) => match str_number(l, s) {
                Some(Num::Int(i)) => Some(BigInt::from(i)),
                Some(Num::Float(f)) => float_integer(f)
                    .map(BigInt::from)
                    .or_else(|| nosql_types::num::parse_varint(String::from_utf8_lossy(s).trim())),
                None => return Err(invalid(NOT_NUMBER)),
            },
            Lv::Dec(d) => decimal_integer(d),
            Lv::Table => return Err(invalid(NOT_NUMBER)),
            _ => return Err(invalid(UNEXPECTED)),
        })
    }
}

/// `value is not an integer` unless integral.
///
/// # Safety
/// A valid state.
unsafe fn varint(l: *mut lua::lua_State, v: &Lv) -> Result<BigInt, Failure> {
    // SAFETY: per the contract.
    unsafe { integral(l, v)?.ok_or_else(|| invalid(NOT_INTEGER)) }
}

/// The low 64 bits of `n` (two's complement).
fn wrap64(n: &BigInt) -> i64 {
    let bytes = n.to_signed_bytes_le();
    let mut b = [if n.is_negative() { 0xff } else { 0 }; 8];
    for (i, x) in bytes.iter().take(8).enumerate() {
        b[i] = *x;
    }
    i64::from_le_bytes(b)
}

/// # Safety
/// A valid state.
unsafe fn number(l: *mut lua::lua_State, v: &Lv) -> Result<f64, Failure> {
    // SAFETY: per the contract.
    unsafe {
        match v {
            Lv::Int(i) => Ok(*i as f64),
            // An integral float goes through Lua's integer conversion first: -0.0 becomes 0.
            Lv::Float(f) if *f == 0.0 => Ok(0.0),
            Lv::Float(f) => Ok(*f),
            Lv::Str(s) => match str_number(l, s) {
                Some(Num::Int(i)) => Ok(i as f64),
                Some(Num::Float(0.0)) => Ok(0.0),
                Some(Num::Float(f)) => Ok(f),
                None => Err(invalid(NOT_NUMBER)),
            },
            Lv::Dec(d) => Ok(d.to_f64()),
            Lv::Table => Err(invalid(NOT_NUMBER)),
            _ => Err(invalid(UNEXPECTED)),
        }
    }
}

/// A decimal with no negative scale, and zero with scale 0 (as ScyllaDB returns decimals).
fn canonical(d: Decimal) -> Decimal {
    if d.unscaled.is_zero() {
        return Decimal::new(BigInt::zero(), 0);
    }
    if d.scale < 0 {
        return Decimal::new(
            d.unscaled * BigInt::from(10u8).pow(d.scale.unsigned_abs()),
            0,
        );
    }
    d
}

/// # Safety
/// A valid state.
unsafe fn decimal(l: *mut lua::lua_State, v: &Lv) -> Result<Decimal, Failure> {
    // SAFETY: per the contract.
    unsafe {
        match v {
            Lv::Int(i) => Ok(Decimal::from_i64(*i)),
            Lv::Float(f) => float_integer(*f)
                .map(Decimal::from_i64)
                .ok_or_else(|| invalid(NOT_DECIMAL)),
            Lv::Str(s) => match str_number(l, s) {
                Some(Num::Int(i)) => Ok(Decimal::from_i64(i)),
                Some(Num::Float(f)) => match float_integer(f) {
                    Some(i) => Ok(Decimal::from_i64(i)),
                    None => std::str::from_utf8(s)
                        .ok()
                        .and_then(Decimal::parse)
                        .map(canonical)
                        .ok_or_else(|| invalid(NOT_DECIMAL)),
                },
                None => Err(invalid(NOT_NUMBER)),
            },
            Lv::Dec(d) => Ok(canonical(d.clone())),
            Lv::Table => Err(invalid(NOT_NUMBER)),
            _ => Err(invalid(UNEXPECTED)),
        }
    }
}

/// Text of a number as `{fmt}` prints a double (shortest round trip; exponent below 1e-4 and
/// from 1e16, at least two exponent digits).
fn float_text(f: f64) -> String {
    if f.is_nan() {
        return if f.is_sign_negative() { "-nan" } else { "nan" }.into();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf" } else { "inf" }.into();
    }
    let e = format!("{f:e}");
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp < 0 {
            out.push_str("0.");
            out.push_str(&"0".repeat((-exp - 1) as usize));
            out.push_str(&digits);
        } else {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.push_str(&"0".repeat(int_len - digits.len()));
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push_str(&format!(
            "e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.unsigned_abs()
        ));
    }
    out
}

/// A decimal's text in the conversions: plain notation, no trailing fraction zeros.
fn decimal_text(d: &Decimal) -> String {
    let d = canonical(d.clone());
    let neg = d.unscaled.is_negative();
    let mut digits = d.unscaled.magnitude().to_string();
    let scale = d.scale as usize;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if scale == 0 {
        out.push_str(&digits);
        return out;
    }
    if digits.len() <= scale {
        digits = format!("{}{digits}", "0".repeat(scale - digits.len() + 1));
    }
    let (int, frac) = digits.split_at(digits.len() - scale);
    let frac = frac.trim_end_matches('0');
    out.push_str(int);
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// The text a value converts to (text, ascii, blob).
fn text_of(v: &Lv) -> Result<Vec<u8>, Failure> {
    match v {
        Lv::Str(s) => Ok(s.clone()),
        Lv::Int(i) => Ok(i.to_string().into_bytes()),
        Lv::Float(f) => Ok(match float_integer(*f) {
            Some(i) => i.to_string(),
            None => float_text(*f),
        }
        .into_bytes()),
        Lv::Dec(d) => Ok(decimal_text(d).into_bytes()),
        _ => Err(invalid(UNEXPECTED)),
    }
}

/// A number or string as Lua's `tostring` shows it (strings as they are).
fn lua_text(v: &Lv) -> Option<String> {
    match v {
        Lv::Str(s) => Some(String::from_utf8_lossy(s).into_owned()),
        Lv::Int(i) => Some(i.to_string()),
        Lv::Float(f) => {
            let mut buf = [0u8; 64];
            // SAFETY: `buf` holds 64 bytes.
            let n = unsafe { lua::nl_fmt_number(*f, buf.as_mut_ptr().cast::<c_char>(), 64) };
            Some(String::from_utf8_lossy(&buf[..n.clamp(0, 63) as usize]).into_owned())
        }
        Lv::Dec(d) => Some(decimal_text(d)),
        _ => None,
    }
}

// ---- Lua → CQL -------------------------------------------------------------------------------

/// The value at absolute index `idx` as `ty` (`Value::Null` for nil).
///
/// # Safety
/// A valid, disarmed state; `idx` absolute.
unsafe fn pull(l: *mut lua::lua_State, idx: c_int, ty: &CqlType) -> Result<Value, Failure> {
    // SAFETY: per the contract; only raw, non-raising accessors are used.
    unsafe {
        if lua::lua_checkstack(l, 6) == 0 {
            return Err(invalid("result is nested too deeply"));
        }
        let v = classify(l, idx);
        if matches!(v, Lv::Nil) {
            return Ok(Value::Null);
        }
        let d = Dialect::Scylla;
        let server = |e: nosql_types::MarshalError| Failure::Server(e.message);
        Ok(match ty {
            CqlType::Ascii => {
                let s = text_of(&v)?;
                if !s.is_ascii() {
                    return Err(invalid("value is not valid ascii"));
                }
                Value::Ascii(String::from_utf8(s).unwrap_or_default())
            }
            CqlType::Text => {
                let s = text_of(&v)?;
                match String::from_utf8(s) {
                    Ok(s) => Value::Text(s),
                    Err(e) => {
                        return Err(Failure::Invalid(format!(
                            "value is not valid utf8, invalid character at byte offset {}",
                            e.utf8_error().valid_up_to()
                        )));
                    }
                }
            }
            CqlType::Blob => Value::Blob(Bytes::from(text_of(&v)?)),
            CqlType::Custom(_) => Value::Custom(Bytes::from(text_of(&v)?)),
            CqlType::Empty => Value::Empty,
            CqlType::Boolean => Value::Boolean(!matches!(v, Lv::Bool(false))),
            CqlType::Tinyint => Value::Tinyint(wrap64(&varint(l, &v)?) as i8),
            CqlType::Smallint => Value::Smallint(wrap64(&varint(l, &v)?) as i16),
            CqlType::Int => Value::Int(wrap64(&varint(l, &v)?) as i32),
            CqlType::Bigint => Value::Bigint(wrap64(&varint(l, &v)?)),
            CqlType::Counter => Value::Counter(wrap64(&varint(l, &v)?)),
            CqlType::Float => Value::Float(number(l, &v)? as f32),
            CqlType::Double => Value::Double(number(l, &v)?),
            CqlType::Varint => Value::Varint(varint(l, &v)?),
            CqlType::Decimal => Value::Decimal(decimal(l, &v)?),
            CqlType::Timestamp => Value::Timestamp(match &v {
                Lv::Str(s) if str_number(l, s).is_none() => {
                    nosql_types::temporal::parse_timestamp(&String::from_utf8_lossy(s), d)
                        .map_err(server)?
                }
                Lv::Table => date_table(l, idx, true)?,
                _ => match integral(l, &v)? {
                    Some(n) => n
                        .to_i64()
                        .ok_or_else(|| invalid("timestamp value must fit in signed 64 bits"))?,
                    None => {
                        return Err(invalid("timestamp must be a string, integer or date table"));
                    }
                },
            }),
            CqlType::Date => Value::Date(match &v {
                Lv::Str(s) if str_number(l, s).is_none() => {
                    nosql_types::temporal::parse_date(&String::from_utf8_lossy(s), d)
                        .map_err(server)?
                }
                Lv::Table => {
                    let days = date_table(l, idx, false)? + nosql_types::temporal::DATE_EPOCH;
                    u32::try_from(days).map_err(|_| invalid("date value must fit in 32 bits"))?
                }
                _ => match integral(l, &v)? {
                    Some(n) if n.is_negative() => {
                        return Err(Failure::Server(
                            "Attempt to assign a negative value to an unsigned type.".into(),
                        ));
                    }
                    Some(n) => n
                        .to_u32()
                        .ok_or_else(|| invalid("date value must fit in 32 bits"))?,
                    None => return Err(invalid("date must be a string, integer or date table")),
                },
            }),
            CqlType::Time => Value::Time(match &v {
                Lv::Str(s) if str_number(l, s).is_none() => {
                    nosql_types::temporal::parse_time(&String::from_utf8_lossy(s), d)
                        .map_err(server)?
                }
                Lv::Table => return Err(invalid("time must be a string or an integer")),
                _ => match integral(l, &v)? {
                    Some(n) => n
                        .to_i64()
                        .ok_or_else(|| invalid("time value must fit in signed 64 bits"))?,
                    None => return Err(invalid("time must be a string or an integer")),
                },
            }),
            CqlType::Duration => match &v {
                Lv::Table => Value::Duration(duration_table(l, idx)?),
                Lv::Str(s) => Value::Duration(
                    nosql_types::Duration::parse(&String::from_utf8_lossy(s), d).map_err(|e| {
                        // The conversion's text has no `marshaling error: ` prefix.
                        let m = e.message;
                        Failure::Server(
                            m.strip_prefix("marshaling error: ")
                                .map(str::to_string)
                                .unwrap_or(m),
                        )
                    })?,
                ),
                Lv::Int(_) | Lv::Float(_) | Lv::Dec(_) => {
                    return Err(invalid(
                        "a duration must be of the form { months = v1, days = v2, nanoseconds = v3 }",
                    ));
                }
                _ => return Err(invalid(UNEXPECTED)),
            },
            CqlType::Inet => {
                let s = lua_text(&v).ok_or_else(|| invalid(UNEXPECTED))?;
                let b = nosql_types::inet::parse_inet(&s, d).ok_or_else(|| {
                    Failure::Server(format!(
                        "marshaling error: Failed to parse inet_addr from '{s}'"
                    ))
                })?;
                Value::Inet(match <[u8; 4]>::try_from(&b[..]) {
                    Ok(v4) => std::net::IpAddr::from(v4),
                    Err(_) => {
                        std::net::IpAddr::from(<[u8; 16]>::try_from(&b[..]).unwrap_or_default())
                    }
                })
            }
            CqlType::Uuid | CqlType::Timeuuid => {
                let s = lua_text(&v).ok_or_else(|| invalid(UNEXPECTED))?;
                let b = nosql_types::from_string(&CqlType::Uuid, &s, d).map_err(server)?;
                if matches!(ty, CqlType::Timeuuid) {
                    nosql_types::validate(ty, &b, d).map_err(server)?;
                }
                let u = <[u8; 16]>::try_from(&b[..]).map_err(|_| {
                    Failure::Server(format!("marshaling error: Cannot parse uuid from '{s}'"))
                })?;
                if matches!(ty, CqlType::Uuid) {
                    Value::Uuid(u)
                } else {
                    Value::Timeuuid(u)
                }
            }
            CqlType::List { elem, .. } => {
                if !matches!(v, Lv::Table) {
                    return Err(invalid(NOT_TABLE));
                }
                let mut items: Vec<(i64, Value)> = Vec::new();
                each(l, idx, |l, k, val| {
                    let key = varint(l, &classify(l, k))?;
                    let item = pull(l, val, elem)?;
                    items.push((key.to_i64().unwrap_or(0), item));
                    Ok(())
                })?;
                items.sort_by_key(|(k, _)| *k);
                if items
                    .iter()
                    .enumerate()
                    .any(|(i, (k, _))| *k != i as i64 + 1)
                {
                    return Err(invalid("table is not a sequence"));
                }
                Value::List(items.into_iter().map(|(_, v)| v).collect())
            }
            CqlType::Vector { elem, dim } => Value::Vector(fixed_sequence(
                l,
                idx,
                &v,
                &vec![(**elem).clone(); *dim as usize],
            )?),
            CqlType::Tuple(types) => Value::Tuple(fixed_sequence(l, idx, &v, types)?),
            CqlType::Set { elem, .. } => {
                if !matches!(v, Lv::Table) {
                    return Err(invalid(NOT_TABLE));
                }
                let mut items: Vec<(Bytes, Value)> = Vec::new();
                each(l, idx, |l, k, val| {
                    if matches!(classify(l, val), Lv::Bool(false)) {
                        return Err(invalid("sets are represented with tables with true values"));
                    }
                    let x = pull(l, k, elem)?;
                    items.push((nosql_types::encode(&x, elem), x));
                    Ok(())
                })?;
                items.sort_by(|a, b| nosql_types::compare(elem, &a.0, &b.0));
                Value::Set(items.into_iter().map(|(_, v)| v).collect())
            }
            CqlType::Map { key, value, .. } => {
                if !matches!(v, Lv::Table) {
                    return Err(invalid(NOT_TABLE));
                }
                let mut items: Vec<(Bytes, Value, Value)> = Vec::new();
                each(l, idx, |l, k, val| {
                    let kv = pull(l, k, key)?;
                    let vv = pull(l, val, value)?;
                    items.push((nosql_types::encode(&kv, key), kv, vv));
                    Ok(())
                })?;
                items.sort_by(|a, b| nosql_types::compare(key, &a.0, &b.0));
                Value::Map(items.into_iter().map(|(_, k, v)| (k, v)).collect())
            }
            CqlType::Udt(def, _) => {
                if !matches!(v, Lv::Table) {
                    return Err(invalid(NOT_TABLE));
                }
                let mut fields: Vec<Option<Value>> = vec![None; def.fields.len()];
                each(l, idx, |l, k, val| {
                    let kv = classify(l, k);
                    let name = match &kv {
                        Lv::Str(s) => String::from_utf8_lossy(s).into_owned(),
                        other => key_text(other),
                    };
                    let pos = match kv {
                        Lv::Str(_) => def.fields.iter().position(|(n, _)| *n == name),
                        _ => None,
                    };
                    let Some(pos) = pos else {
                        return Err(Failure::Invalid(format!("invalid UDT field '{name}'")));
                    };
                    fields[pos] = Some(pull(l, val, &def.fields[pos].1)?);
                    Ok(())
                })?;
                let mut out = Vec::with_capacity(fields.len());
                for (f, (name, _)) in fields.into_iter().zip(&def.fields) {
                    match f {
                        Some(v) => out.push(v),
                        None => {
                            return Err(Failure::Invalid(format!(
                                "key {name} missing in udt {}",
                                def.name
                            )));
                        }
                    }
                }
                Value::Udt(out)
            }
        })
    }
}

/// A table key as messages show it.
fn key_text(v: &Lv) -> String {
    match v {
        Lv::Bool(b) => b.to_string(),
        Lv::Table => "table".into(),
        Lv::Other(t) => type_name(*t).into(),
        Lv::Nil => "nil".into(),
        other => lua_text(other).unwrap_or_default(),
    }
}

/// A tuple or vector: keys `1..=n` (`key k is not valid for a sequence of size n`), all
/// present (`key k missing in sequence of size n`).
///
/// # Safety
/// A valid state and absolute index.
unsafe fn fixed_sequence(
    l: *mut lua::lua_State,
    idx: c_int,
    v: &Lv,
    types: &[CqlType],
) -> Result<Vec<Value>, Failure> {
    // SAFETY: per the contract.
    unsafe {
        if !matches!(v, Lv::Table) {
            return Err(invalid(NOT_TABLE));
        }
        let n = types.len();
        let mut items: Vec<Option<Value>> = vec![None; n];
        each(l, idx, |l, k, val| {
            let key = varint(l, &classify(l, k))?;
            let pos = key
                .to_usize()
                .filter(|p| (1..=n).contains(p))
                .ok_or_else(|| {
                    Failure::Invalid(format!("key {key} is not valid for a sequence of size {n}"))
                })?;
            items[pos - 1] = Some(pull(l, val, &types[pos - 1])?);
            Ok(())
        })?;
        let mut out = Vec::with_capacity(n);
        for (i, item) in items.into_iter().enumerate() {
            match item {
                Some(v) => out.push(v),
                None => {
                    return Err(Failure::Invalid(format!(
                        "key {} missing in sequence of size {n}",
                        i + 1
                    )));
                }
            }
        }
        Ok(out)
    }
}

/// A date table (`year`, `month`, `day`, and for timestamps `hour` (12 when absent, as
/// `os.time`), `min`, `sec`; `isdst` is read as an integer and ignored), normalized like
/// `timegm`: days since the epoch, or milliseconds for a timestamp.
///
/// # Safety
/// A valid state and absolute index of a table.
unsafe fn date_table(l: *mut lua::lua_State, idx: c_int, with_time: bool) -> Result<i64, Failure> {
    // SAFETY: per the contract.
    unsafe {
        let mut f: [Option<i64>; 6] = [None; 6];
        let mut has_time = false;
        each(l, idx, |l, k, val| {
            let kv = classify(l, k);
            let name = match &kv {
                Lv::Str(s) => String::from_utf8_lossy(s).into_owned(),
                other => key_text(other),
            };
            let slot = match (&kv, name.as_str()) {
                (Lv::Str(_), "year") => Some(0),
                (Lv::Str(_), "month") => Some(1),
                (Lv::Str(_), "day") => Some(2),
                (Lv::Str(_), "hour") => Some(3),
                (Lv::Str(_), "min") => Some(4),
                (Lv::Str(_), "sec") => Some(5),
                (Lv::Str(_), "isdst") => None,
                _ => {
                    return Err(Failure::Invalid(format!(
                        "invalid date table field: '{name}'"
                    )));
                }
            };
            let n = varint(l, &classify(l, val))?;
            let n = n
                .to_i32()
                .ok_or_else(|| Failure::Invalid(format!("{name} is too large: '{n}'")))?;
            if let Some(s) = slot {
                if s >= 3 {
                    has_time = true;
                }
                f[s] = Some(n as i64);
            }
            Ok(())
        })?;
        let (Some(y), Some(m), Some(d)) = (f[0], f[1], f[2]) else {
            return Err(invalid("date table must have year, month and day"));
        };
        if has_time && !with_time {
            return Err(invalid("date type has no hour, minute or second"));
        }
        let m0 = m - 1;
        let y = y + m0.div_euclid(12);
        let m = (m0.rem_euclid(12) + 1) as u32;
        let days = nosql_types::temporal::days_from_civil(y, m, 1) + (d - 1);
        if !with_time {
            return Ok(days);
        }
        let secs = days as i128 * 86_400
            + f[3].unwrap_or(12) as i128 * 3600
            + f[4].unwrap_or(0) as i128 * 60
            + f[5].unwrap_or(0) as i128;
        i64::try_from(secs * 1000)
            .map_err(|_| invalid("timestamp value must fit in signed 64 bits"))
    }
}

/// A duration table `{months, days, nanoseconds}` (any subset).
///
/// # Safety
/// A valid state and absolute index of a table.
unsafe fn duration_table(
    l: *mut lua::lua_State,
    idx: c_int,
) -> Result<nosql_types::Duration, Failure> {
    // SAFETY: per the contract.
    unsafe {
        let (mut months, mut days, mut nanos) = (0i32, 0i32, 0i64);
        each(l, idx, |l, k, val| {
            let kv = classify(l, k);
            let name = match &kv {
                Lv::Str(s) => String::from_utf8_lossy(s).into_owned(),
                other => key_text(other),
            };
            let field = match (&kv, name.as_str()) {
                (Lv::Str(_), "months") => 0,
                (Lv::Str(_), "days") => 1,
                (Lv::Str(_), "nanoseconds") => 2,
                _ => {
                    return Err(Failure::Invalid(format!(
                        "invalid duration field: '{name}'"
                    )));
                }
            };
            let n = varint(l, &classify(l, val))?;
            let narrow = |n: &BigInt| {
                n.to_i32().ok_or_else(|| {
                    Failure::Invalid(format!("{n} {name} doesn't fit in a 32 bit integer"))
                })
            };
            match field {
                0 => months = narrow(&n)?,
                1 => days = narrow(&n)?,
                _ => {
                    nanos = n.to_i64().ok_or_else(|| {
                        Failure::Invalid(format!("{n} {name} doesn't fit in a 64 bit integer"))
                    })?
                }
            }
            Ok(())
        })?;
        Ok(nosql_types::Duration::new(months, days, nanos))
    }
}

/// Calls `f(l, key index, value index)` for every entry of the table at `idx`, stopping at the
/// first error.
///
/// # Safety
/// A valid state and absolute index of a table; `f` must leave the stack as it found it and
/// must not convert the key in place.
unsafe fn each(
    l: *mut lua::lua_State,
    idx: c_int,
    mut f: impl FnMut(*mut lua::lua_State, c_int, c_int) -> Result<(), Failure>,
) -> Result<(), Failure> {
    // SAFETY: per the contract; `lua_next` doesn't raise for keys present in the table.
    unsafe {
        if lua::lua_checkstack(l, 3) == 0 {
            return Err(invalid("result is nested too deeply"));
        }
        lua::lua_pushnil(l);
        while lua::lua_next(l, idx) != 0 {
            let top = lua::lua_gettop(l);
            let r = f(l, top - 1, top);
            lua::lua_settop(l, top - 1);
            if r.is_err() {
                lua::lua_pop(l, 1);
                return r;
            }
        }
        Ok(())
    }
}

// ---- Scylla.decimal arithmetic ---------------------------------------------------------------

#[derive(Clone, Copy)]
enum Op {
    Add,
    Sub,
}

unsafe extern "C" fn decimal_add(l: *mut lua::lua_State, out: *mut lua::nl_slice) -> c_int {
    // SAFETY: called by the trampoline on a valid state with the two operands.
    unsafe { arith(l, out, Op::Add) }
}

unsafe extern "C" fn decimal_sub(l: *mut lua::lua_State, out: *mut lua::nl_slice) -> c_int {
    // SAFETY: as above.
    unsafe { arith(l, out, Op::Sub) }
}

/// `__add` / `__sub` of `Scylla.decimal`: floats when an operand is a float, else exact decimals
/// (the larger scale); `__sub` subtracts from the decimal whichever side it is on, as ScyllaDB
/// does. A conversion error fails the whole call (ScyllaDB raises it past Lua).
///
/// # Safety
/// A valid state whose bounds' host is a [`Host`]; non-raising calls only.
unsafe fn arith(l: *mut lua::lua_State, out: *mut lua::nl_slice, op: Op) -> c_int {
    // SAFETY: per the contract; `out` is the trampoline's.
    unsafe {
        let bounds = lua::nl_bounds_of(l);
        let host = &mut *(*bounds).host.cast::<Host>();
        let mut a = classify(l, 1);
        let mut b = classify(l, 2);
        // ScyllaDB takes the decimal as the left operand: `1 - x` is `x - 1`.
        if matches!(op, Op::Sub) && !matches!(a, Lv::Dec(_)) {
            std::mem::swap(&mut a, &mut b);
        }
        let result = if matches!(a, Lv::Float(_)) || matches!(b, Lv::Float(_)) {
            match (number(l, &a), number(l, &b)) {
                (Ok(x), Ok(y)) => {
                    lua::lua_pushnumber(
                        l,
                        match op {
                            Op::Add => x + y,
                            Op::Sub => x - y,
                        },
                    );
                    return 1;
                }
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        } else {
            let operand = |v: &Lv| -> Result<Decimal, Failure> {
                match v {
                    Lv::Str(s) => match str_number(l, s) {
                        None => Err(invalid(NOT_NUMBER)),
                        Some(_) => decimal(l, v),
                    },
                    other => decimal(l, other),
                }
            };
            match (operand(&a), operand(&b)) {
                (Ok(x), Ok(y)) => Ok(decimal_op(&x, &y, op)),
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        };
        match result {
            Ok(d) => {
                host.scratch = d.to_bytes();
                (*out).ptr = host.scratch.as_ptr().cast::<c_char>();
                (*out).len = host.scratch.len();
                lua::NL_DECIMAL
            }
            Err(e) => {
                host.scratch = e.to_string().into_bytes();
                if host.fatal.is_none() {
                    host.fatal = Some(e);
                }
                (*bounds).expired = 1;
                (*out).ptr = host.scratch.as_ptr().cast::<c_char>();
                (*out).len = host.scratch.len();
                lua::NL_RAISE
            }
        }
    }
}

fn decimal_op(a: &Decimal, b: &Decimal, op: Op) -> Decimal {
    let scale = a.scale.max(b.scale);
    let rescale = |d: &Decimal| {
        let up = (scale as i64 - d.scale as i64) as u32;
        &d.unscaled * BigInt::from(10u8).pow(up)
    };
    let (x, y) = (rescale(a), rescale(b));
    Decimal::new(
        match op {
            Op::Add => x + y,
            Op::Sub => x - y,
        },
        scale,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        args: &[&str],
        body: &str,
        values: &[(&CqlType, Option<&[u8]>)],
        ret: &CqlType,
    ) -> Result<Option<Bytes>, Failure> {
        let names: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let code = compile(&names, body).map_err(Failure::Runtime)?;
        call(&code, values, ret)
    }

    #[test]
    fn integers_and_text() {
        let four = 4i32.to_be_bytes();
        let r = run(
            &["x"],
            "return x + 1",
            &[(&CqlType::Int, Some(&four))],
            &CqlType::Int,
        )
        .unwrap();
        assert_eq!(r.unwrap()[..], 5i32.to_be_bytes());
        let r = run(
            &["x"],
            "return x * 100",
            &[(&CqlType::Int, Some(&four))],
            &CqlType::Tinyint,
        );
        assert_eq!(r.unwrap().unwrap()[..], [144u8]);
        let r = run(&[], "return 9007199254740993", &[], &CqlType::Bigint).unwrap();
        assert_eq!(r.unwrap()[..], 9007199254740993i64.to_be_bytes());
        let r = run(
            &[],
            "return 7 // 2 .. ' ' .. tostring(2.0)",
            &[],
            &CqlType::Text,
        )
        .unwrap();
        assert_eq!(&r.unwrap()[..], b"3 2.0");
        assert_eq!(run(&[], "return nil", &[], &CqlType::Int).unwrap(), None);
        assert_eq!(
            run(&[], "return 1, 2", &[], &CqlType::Int).unwrap_err(),
            Failure::Invalid("2 values returned, expected 1".into())
        );
    }

    #[test]
    fn float_and_decimal_text() {
        for (f, s) in [
            (1e30, "1e+30"),
            (1.5, "1.5"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e-5, "1e-05"),
            (1e-4, "0.0001"),
            (1.2345e20, "1.2345e+20"),
            (5e-324, "5e-324"),
            (-2.5e-7, "-2.5e-07"),
            (1234567890123456.5, "1234567890123456.5"),
            (f64::NEG_INFINITY, "-inf"),
        ] {
            assert_eq!(float_text(f), s);
        }
        assert_eq!(float_integer(9_223_372_036_854_775_808.0), Some(i64::MIN));
        assert_eq!(float_integer(-1e30), None);
        let d = |s| Decimal::parse(s).unwrap();
        assert_eq!(decimal_text(&d("1.50")), "1.5");
        assert_eq!(decimal_text(&d("1.5E+3")), "1500");
        assert_eq!(decimal_text(&d("-0.000")), "0");
        assert_eq!(decimal_text(&d("1E-10")), "0.0000000001");
    }

    #[test]
    fn errors_and_limits() {
        assert_eq!(
            compile(&[], "return ("),
            Err("[string \"<internal>\"]:1: unexpected symbol near <eof>".into())
        );
        assert_eq!(
            run(&[], "error('boom')", &[], &CqlType::Int).unwrap_err(),
            Failure::Runtime("boom".into())
        );
        assert!(matches!(
            run(&[], "while true do end", &[], &CqlType::Int),
            Err(Failure::Timeout(_))
        ));
        assert!(matches!(
            run(
                &[],
                "while true do pcall(function() while true do end end) end",
                &[],
                &CqlType::Int
            ),
            Err(Failure::Timeout(_))
        ));
        assert_eq!(
            run(
                &[],
                "local t = {} for i = 1, 1e8 do t[i] = i end return 1",
                &[],
                &CqlType::Int
            )
            .unwrap_err(),
            Failure::Runtime("not enough memory".into())
        );
        assert_eq!(
            run(&[], "return dofile('/etc/passwd')", &[], &CqlType::Int).unwrap_err(),
            Failure::Runtime("cannot open /etc/passwd: No such file or directory".into())
        );
        assert!(
            run(
                &[],
                "return (load(string.dump(function() end)))",
                &[],
                &CqlType::Int
            )
            .is_ok()
        );
    }

    #[test]
    fn decimals() {
        let vi = CqlType::Varint;
        let b = BigInt::from(10).pow(30).to_signed_bytes_be();
        let r = run(&["x"], "return x + 1", &[(&vi, Some(&b))], &vi)
            .unwrap()
            .unwrap();
        assert_eq!(
            BigInt::from_signed_bytes_be(&r),
            BigInt::from(10).pow(30) + 1
        );
        assert_eq!(
            run(&["x"], "return x + 'abc'", &[(&vi, Some(&b))], &vi).unwrap_err(),
            Failure::Invalid(NOT_NUMBER.into())
        );
        assert_eq!(
            run(
                &["x"],
                "pcall(function() return x + {} end) return 1",
                &[(&vi, Some(&b))],
                &vi
            )
            .unwrap_err(),
            Failure::Invalid(NOT_NUMBER.into())
        );
    }
}
