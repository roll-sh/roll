//! The Lua debugger (LDB). After SCRIPT DEBUG YES or SYNC, the client's next EVAL is a debugging
//! session: the script stops at its first line and on breakpoints, and the client drives it with
//! debugger commands (step, continue, list, print, break, eval, redis, trace, maxlen, abort),
//! sent as commands and answered with arrays of status lines. The session ends with an
//! `<endsession>` line, the EVAL's reply and the connection closing.
//!
//! In SYNC mode the session runs in place, holding the server; in YES mode Redis forks and the
//! child runs it on a copy of the data, so its changes are discarded and the parent goes on (the
//! connection layer runs it on a copy of the engine). The session talks to the client directly
//! through [`LdbIo`], blocking, as Redis reads and writes its socket from inside the Lua hook.

use super::vm::{push_error, run_ctx, to_bytes};
use crate::util::{fmt_g, parse_i64};
use bytes::BytesMut;
use redis_lua::*;
use redis_proto::Reply;
use std::ffi::{CStr, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

/// `LDB_MAX_LEN_DEFAULT`.
const MAX_LEN_DEFAULT: usize = 256;
/// `LDB_BREAKPOINTS_MAX`.
const BREAKPOINTS_MAX: usize = 64;
/// `LDB_MAX_VALUES_DEPTH` (`LUA_MINSTACK / 2`).
const MAX_VALUES_DEPTH: usize = 10;

/// What SCRIPT DEBUG set for a client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LdbMode {
    #[default]
    Off,
    /// YES: sessions run on a copy of the data (a fork in Redis).
    Fork,
    Sync,
}

/// The debugging client's connection, used blocking while the script is stopped.
pub trait LdbIo {
    /// Up to `buf.len()` bytes from the client; 0 once it is gone.
    fn read(&mut self, buf: &mut [u8]) -> usize;
    /// Write to the client (errors show up as the next read failing).
    fn write(&mut self, data: &[u8]);
}

/// The connection of the call in progress.
pub(crate) struct IoPtr(pub(crate) *mut (dyn LdbIo + 'static));

// SAFETY: set only for the duration of one call on the thread making it.
unsafe impl Send for IoPtr {}

/// The debugger's state (Redis keeps one per process).
#[derive(Default)]
pub struct Ldb {
    pub(crate) active: bool,
    /// The session runs on a copy of the data.
    pub(crate) forked: bool,
    /// Stop at the next line.
    pub(crate) step: bool,
    /// `redis.breakpoint()` was called.
    pub(crate) luabp: bool,
    bp: Vec<i32>,
    maxlen: usize,
    maxlen_hint_sent: bool,
    logs: Vec<Vec<u8>>,
    src: Vec<Vec<u8>>,
    pub(crate) currentline: i32,
    cbuf: Vec<u8>,
    pub(crate) io: Option<IoPtr>,
}

impl Ldb {
    /// `ldbEnable`.
    pub(crate) fn enable(&mut self) {
        self.logs.clear();
        self.step = true;
        self.bp.clear();
        self.luabp = false;
        self.cbuf.clear();
        self.maxlen = MAX_LEN_DEFAULT;
        self.maxlen_hint_sent = false;
    }

    /// What a forked child inherits: everything but the connection.
    pub(crate) fn copy_settings(&self) -> Ldb {
        Ldb {
            step: self.step,
            luabp: self.luabp,
            bp: self.bp.clone(),
            maxlen: self.maxlen,
            maxlen_hint_sent: self.maxlen_hint_sent,
            logs: self.logs.clone(),
            cbuf: self.cbuf.clone(),
            ..Ldb::default()
        }
    }

    /// `ldbIsEnabled`: commands, replies and errors get logged.
    pub(crate) fn logging(&self) -> bool {
        self.active && self.step
    }

    pub(crate) fn log(&mut self, entry: impl Into<Vec<u8>>) {
        self.logs.push(entry.into());
    }

    /// `ldbLogWithMaxLen`.
    fn log_trimmed(&mut self, mut entry: Vec<u8>) {
        let trimmed = self.maxlen > 0 && entry.len() > self.maxlen;
        if trimmed {
            entry.truncate(self.maxlen);
            entry.extend_from_slice(b" ...");
        }
        self.log(entry);
        if trimmed && !self.maxlen_hint_sent {
            self.maxlen_hint_sent = true;
            self.log("<hint> The above reply was trimmed. Use 'maxlen 0' to disable trimming.");
        }
    }

    /// `ldbSendLogs`: the pending lines as an array of status replies.
    pub(crate) fn send_logs(&mut self) {
        let mut out = format!("*{}\r\n", self.logs.len()).into_bytes();
        for mut line in self.logs.drain(..) {
            for c in line.iter_mut() {
                if *c == b'\r' || *c == b'\n' {
                    *c = b' ';
                }
            }
            out.push(b'+');
            out.extend_from_slice(&line);
            out.extend_from_slice(b"\r\n");
        }
        if let Some(io) = &self.io {
            // SAFETY: see `IoPtr`.
            unsafe { (*io.0).write(&out) };
        }
    }

    /// `ldbStartSession`'s source split: trailing newlines dropped, one entry per line.
    pub(crate) fn start(&mut self, forked: bool, body: &[u8]) {
        self.forked = forked;
        self.active = true;
        let mut end = body.len();
        while end > 0 && matches!(body[end - 1], b'\n' | b'\r') {
            end -= 1;
        }
        self.src = if end == 0 { Vec::new() } else { body[..end].split(|&c| c == b'\n').map(<[u8]>::to_vec).collect() };
    }

    /// `ldbEndSession`'s log: the `<endsession>` mark after whatever is left.
    pub(crate) fn end(&mut self) {
        self.log("<endsession>");
        self.send_logs();
        self.src.clear();
        self.active = false;
    }

    fn source_line(&self, line: i32) -> &[u8] {
        match usize::try_from(line - 1).ok().and_then(|i| self.src.get(i)) {
            Some(l) => l,
            None => b"<out of range source code line>",
        }
    }

    fn is_bp(&self, line: i32) -> bool {
        self.bp.contains(&line)
    }

    /// `ldbLogSourceLine`.
    fn log_source_line(&mut self, line: i32) {
        let prefix = match (self.currentline == line, self.is_bp(line)) {
            (true, true) => "->#",
            (true, false) => "-> ",
            (false, true) => "  #",
            (false, false) => "   ",
        };
        let mut s = format!("{prefix}{line:<3} ").into_bytes();
        s.extend_from_slice(self.source_line(line));
        self.log(s);
    }

    /// `ldbList`: everything, or `context` lines around `around`.
    fn list(&mut self, around: i32, context: i32) {
        for j in 1..=self.src.len() as i32 {
            if around != 0 && (around - j).abs() > context {
                continue;
            }
            self.log_source_line(j);
        }
    }

    /// `ldbBreak`.
    fn break_cmd(&mut self, argv: &[Vec<u8>]) {
        if argv.len() == 1 {
            if self.bp.is_empty() {
                self.log("No breakpoints set. Use 'b <line>' to add one.");
            } else {
                self.log(format!("{} breakpoints set:", self.bp.len()));
                for line in self.bp.clone() {
                    self.log_source_line(line);
                }
            }
            return;
        }
        for arg in &argv[1..] {
            match parse_i64(arg).filter(|&n| i32::try_from(n).is_ok()).map(|n| n as i32) {
                None => {
                    let mut s = b"Invalid argument:'".to_vec();
                    s.extend_from_slice(arg);
                    s.push(b'\'');
                    self.log(s);
                }
                Some(0) => {
                    self.bp.clear();
                    self.log("All breakpoints removed.");
                }
                Some(line) if line > 0 => {
                    if self.bp.len() == BREAKPOINTS_MAX {
                        self.log("Too many breakpoints set.");
                    } else if line as usize <= self.src.len() && !self.is_bp(line) {
                        self.bp.push(line);
                        self.list(line, 1);
                    } else {
                        self.log("Wrong line number.");
                    }
                }
                Some(line) => match self.bp.iter().position(|&b| b == -line) {
                    Some(i) => {
                        self.bp.remove(i);
                        self.log("Breakpoint removed.");
                    }
                    None => self.log("No breakpoint in the specified line."),
                },
            }
        }
    }

    /// `ldbMaxlen`.
    fn maxlen_cmd(&mut self, argv: &[Vec<u8>]) {
        if argv.len() == 2 {
            let n = atoi(&argv[1]);
            self.maxlen_hint_sent = true;
            self.maxlen = if n != 0 && n <= 60 { 60 } else { n.max(0) as usize };
        }
        if self.maxlen > 0 {
            self.log(format!("<value> replies are truncated at {} bytes.", self.maxlen));
        } else {
            self.log("<value> replies are unlimited.");
        }
    }

    /// `ldbLogRedisReply`: a command's reply, as the script client got it.
    pub(crate) fn log_reply(&mut self, reply: &Reply, resp: u8) {
        let mut raw = BytesMut::new();
        reply.encode(resp, &mut raw);
        let mut s = b"<reply> ".to_vec();
        human(&mut s, &raw, 0);
        self.log_trimmed(s);
    }
}

/// C's `atoi`: leading spaces, a sign, digits; 0 when there are none.
fn atoi(s: &[u8]) -> i64 {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut v: i64 = 0;
    while let Some(&c) = s.get(i).filter(|c| c.is_ascii_digit()) {
        v = v.wrapping_mul(10).wrapping_add((c - b'0') as i64);
        i += 1;
    }
    if neg { v.wrapping_neg() } else { v }
}

/// `sdscatrepr`: quoted, with C escapes for the rest of the non-printable bytes.
pub(crate) fn repr(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for &c in s {
        match c {
            b'\\' | b'"' => out.extend_from_slice(&[b'\\', c]),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x20..=0x7e => out.push(c),
            _ => out.extend_from_slice(format!("\\x{c:02x}").as_bytes()),
        }
    }
    out.push(b'"');
}

/// `ldbRedisProtocolToHuman`: the reply at `p` of `r` appended readably; returns where it ends
/// (unknown types are left as they are).
fn human(o: &mut Vec<u8>, r: &[u8], p: usize) -> usize {
    let cr = |from: usize| r[from..].iter().position(|&c| c == b'\r').map_or(r.len(), |i| from + i);
    let num = |a: usize, b: usize| parse_i64(&r[a..b]).unwrap_or(0);
    let Some(&t) = r.get(p) else { return p };
    match t {
        b':' => {
            let e = cr(p + 1);
            o.extend_from_slice(&r[p + 1..e]);
            e + 2
        }
        b'$' => {
            let e = cr(p + 1);
            let n = num(p + 1, e);
            if n == -1 {
                o.extend_from_slice(b"NULL");
                e + 2
            } else {
                let start = (e + 2).min(r.len());
                let end = (start + n.max(0) as usize).min(r.len());
                repr(o, &r[start..end]);
                end + 2
            }
        }
        b'+' | b'-' => {
            let e = cr(p + 1);
            repr(o, &r[p..e]);
            e + 2
        }
        b'*' | b'~' | b'%' => {
            let e = cr(p + 1);
            let n = num(p + 1, e);
            let mut q = e + 2;
            if t == b'*' && n == -1 {
                o.extend_from_slice(b"NULL");
                return q;
            }
            let (open, close): (&[u8], &[u8]) = match t {
                b'*' => (b"[", b"]"),
                b'~' => (b"~(", b")"),
                _ => (b"{", b"}"),
            };
            o.extend_from_slice(open);
            for j in 0..n.max(0) {
                q = human(o, r, q);
                if t == b'%' {
                    o.extend_from_slice(b" => ");
                    q = human(o, r, q);
                }
                if j != n - 1 {
                    o.push(b',');
                }
            }
            o.extend_from_slice(close);
            q
        }
        b'_' => {
            o.extend_from_slice(b"(null)");
            cr(p + 1) + 2
        }
        b'#' => {
            o.extend_from_slice(if r.get(p + 1) == Some(&b't') { b"#true" } else { b"#false" });
            cr(p + 1) + 2
        }
        b',' => {
            let e = cr(p + 1);
            o.extend_from_slice(b"(double) ");
            o.extend_from_slice(&r[p + 1..e]);
            e + 2
        }
        _ => p,
    }
}

/// `ldbCatStackValue`: the Lua value at `idx`, readably.
pub(crate) unsafe fn cat_value(out: &mut Vec<u8>, l: *mut lua_State, idx: c_int) {
    unsafe {
        let idx = if idx < 0 && idx > LUA_REGISTRYINDEX { lua_gettop(l) + idx + 1 } else { idx };
        cat_value_rec(out, l, idx, 0)
    }
}

unsafe fn cat_value_rec(out: &mut Vec<u8>, l: *mut lua_State, idx: c_int, level: usize) {
    if level == MAX_VALUES_DEPTH {
        out.extend_from_slice(b"<max recursion level reached! Nested table?>");
        return;
    }
    let level = level + 1;
    unsafe {
        let t = lua_type(l, idx);
        match t {
            LUA_TSTRING => repr(out, to_bytes(l, idx).unwrap_or_default()),
            LUA_TBOOLEAN => out.extend_from_slice(if lua_toboolean(l, idx) != 0 { b"true" } else { b"false" }),
            LUA_TNUMBER => out.extend_from_slice(fmt_g(lua_tonumber(l, idx)).as_bytes()),
            LUA_TNIL => out.extend_from_slice(b"nil"),
            LUA_TTABLE => {
                // Both renderings at once: as an array (1, 2, 3... keys) or with every key.
                let mut expected = 1.0;
                let mut is_array = true;
                let (mut as_array, mut full) = (Vec::new(), Vec::new());
                lua_pushnil(l);
                while lua_next(l, idx) != 0 {
                    let (k, v) = (lua_gettop(l) - 1, lua_gettop(l));
                    if is_array && (lua_type(l, k) != LUA_TNUMBER || lua_tonumber(l, k) != expected) {
                        is_array = false;
                    }
                    cat_value_rec(&mut as_array, l, v, level);
                    as_array.extend_from_slice(b"; ");
                    full.push(b'[');
                    cat_value_rec(&mut full, l, k, level);
                    full.extend_from_slice(b"]=");
                    cat_value_rec(&mut full, l, v, level);
                    full.extend_from_slice(b"; ");
                    lua_pop(l, 1);
                    expected += 1.0;
                }
                for s in [&mut as_array, &mut full] {
                    s.truncate(s.len().saturating_sub(2));
                }
                out.push(b'{');
                out.extend_from_slice(if is_array { &as_array } else { &full });
                out.push(b'}');
            }
            LUA_TFUNCTION | LUA_TUSERDATA | LUA_TTHREAD | LUA_TLIGHTUSERDATA => {
                let name = match t {
                    LUA_TFUNCTION => "function",
                    LUA_TUSERDATA => "userdata",
                    LUA_TTHREAD => "thread",
                    _ => "light-userdata",
                };
                out.extend_from_slice(format!("\"{name}@{:p}\"", lua_topointer(l, idx)).as_bytes());
            }
            _ => out.extend_from_slice(b"\"<unknown-lua-type>\""),
        }
    }
}

/// The running session's state, through the script's run context.
unsafe fn ldb<'a>(l: *mut lua_State) -> Option<&'a mut Ldb> {
    unsafe { run_ctx(l).map(|run| &mut run.cx().eng.scripting.ldb) }
}

/// `ldbLogStackValue` of the stack top.
unsafe fn log_top(l: *mut lua_State, prefix: &[u8]) {
    unsafe {
        let mut s = prefix.to_vec();
        cat_value(&mut s, l, -1);
        if let Some(d) = ldb(l) {
            d.log_trimmed(s);
        }
    }
}

/// A Lua frame `level` up: its current line, short source and name.
unsafe fn frame(l: *mut lua_State, level: c_int) -> Option<(i32, Vec<u8>, Option<Vec<u8>>)> {
    unsafe {
        let (mut line, mut src, mut name): (c_int, *const c_char, *const c_char) = (0, std::ptr::null(), std::ptr::null());
        if roll_frame(l, level, &mut line, &mut src, &mut name) == 0 {
            return None;
        }
        let src = if src.is_null() { Vec::new() } else { CStr::from_ptr(src).to_bytes().to_vec() };
        let name = (!name.is_null()).then(|| CStr::from_ptr(name).to_bytes().to_vec());
        Some((line, src, name))
    }
}

fn is_user_script(src: &[u8]) -> bool {
    src.windows(11).any(|w| w == b"user_script")
}

/// `ldbPrint`: the first local of that name from the current frame outwards, else the KEYS or
/// ARGV global.
unsafe fn print_var(l: *mut lua_State, var: &[u8]) {
    unsafe {
        let mut level = 0;
        while frame(l, level).is_some() {
            let mut i = 1;
            loop {
                let name = roll_getlocal(l, level, i);
                if name.is_null() {
                    break;
                }
                i += 1;
                if CStr::from_ptr(name).to_bytes() == var {
                    log_top(l, b"<value> ");
                    lua_pop(l, 1);
                    return;
                }
                lua_pop(l, 1);
            }
            level += 1;
        }
        if var == b"ARGV" || var == b"KEYS" {
            let name = std::ffi::CString::new(var).unwrap();
            lua_getglobal(l, name.as_ptr());
            log_top(l, b"<value> ");
            lua_pop(l, 1);
        } else if let Some(d) = ldb(l) {
            d.log("No such variable.");
        }
    }
}

/// `ldbPrintAll`: the current frame's locals.
unsafe fn print_all(l: *mut lua_State) {
    unsafe {
        let mut vars = 0;
        if frame(l, 0).is_some() {
            let mut i = 1;
            loop {
                let name = roll_getlocal(l, 0, i);
                if name.is_null() {
                    break;
                }
                i += 1;
                let name = CStr::from_ptr(name).to_bytes();
                if !name.windows(12).any(|w| w == b"(*temporary)") {
                    let mut prefix = b"<value> ".to_vec();
                    prefix.extend_from_slice(name);
                    prefix.extend_from_slice(b" = ");
                    log_top(l, &prefix);
                    vars += 1;
                }
                lua_pop(l, 1);
            }
        }
        if vars == 0
            && let Some(d) = ldb(l)
        {
            d.log("No local variables in the current context.");
        }
    }
}

/// `ldbTrace`: the frames of the script, innermost first.
unsafe fn trace(l: *mut lua_State) {
    unsafe {
        let mut level = 0;
        while let Some((line, src, name)) = frame(l, level) {
            if is_user_script(&src)
                && let Some(d) = ldb(l)
            {
                let mut s = if level == 0 { b"In ".to_vec() } else { b"From ".to_vec() };
                s.extend_from_slice(name.as_deref().unwrap_or(b"top level"));
                s.push(b':');
                d.log(s);
                d.log_source_line(line);
            }
            level += 1;
        }
        if level == 0
            && let Some(d) = ldb(l)
        {
            d.log("<error> Can't retrieve Lua stack.");
        }
    }
}

/// `ldbEval`: the code as an expression (`return ...`), else as a statement, in a new frame.
unsafe fn eval(l: *mut lua_State, argv: &[Vec<u8>]) {
    unsafe {
        let code = argv[1..].join(&b' ');
        let mut expr = b"return ".to_vec();
        expr.extend_from_slice(&code);
        let name = c"@ldb_eval".as_ptr();
        if luaL_loadbuffer(l, expr.as_ptr().cast(), expr.len(), name) != 0 {
            lua_pop(l, 1);
            if luaL_loadbuffer(l, code.as_ptr().cast(), code.len(), name) != 0 {
                let mut s = b"<error> ".to_vec();
                s.extend_from_slice(c_str(l, -1));
                lua_pop(l, 1);
                if let Some(d) = ldb(l) {
                    d.log(s);
                }
                return;
            }
        }
        if lua_pcall(l, 0, 1, 0) != 0 {
            let mut s = b"<error> ".to_vec();
            if lua_isstring(l, -1) != 0 {
                s.extend_from_slice(c_str(l, -1));
            } else {
                s.extend_from_slice(b"(error object is not a string)");
            }
            lua_pop(l, 1);
            if let Some(d) = ldb(l) {
                d.log(s);
            }
            return;
        }
        log_top(l, b"<retval> ");
        lua_pop(l, 1);
    }
}

/// A Lua string as C sees it (up to a NUL).
unsafe fn c_str<'a>(l: *mut lua_State, idx: c_int) -> &'a [u8] {
    let b = unsafe { to_bytes(l, idx).unwrap_or_default() };
    &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]
}

/// `ldbRedis`: the command through `redis.call` with logging on, its result dropped.
unsafe fn redis(l: *mut lua_State, argv: &[Vec<u8>]) {
    unsafe {
        if lua_checkstack(l, argv.len() as c_int + 1) == 0 {
            if let Some(d) = ldb(l) {
                d.log_trimmed(b"<reply> ".to_vec());
            }
            return;
        }
        lua_getglobal(l, c"redis".as_ptr());
        lua_pushstring(l, c"call".as_ptr());
        lua_gettable(l, -2);
        for a in &argv[1..] {
            lua_pushlstring(l, a.as_ptr().cast(), a.len());
        }
        if let Some(d) = ldb(l) {
            d.step = true;
        }
        lua_pcall(l, argv.len() as c_int - 1, 1, 0);
        if let Some(d) = ldb(l) {
            d.step = false;
        }
        lua_pop(l, 2);
    }
}

enum Parsed {
    Command(Vec<Vec<u8>>),
    More,
    Error,
}

/// `ldbReplParseCommand`: one multi-bulk command from the buffer (forgiving, as Redis is).
fn parse_command(buf: &[u8]) -> Parsed {
    if buf.is_empty() {
        return Parsed::More;
    }
    // C string searches: they stop at a NUL.
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let find = |from: usize, pat: &[u8]| -> Option<usize> {
        if from > end {
            return None;
        }
        buf[from..end].windows(pat.len()).position(|w| w == pat).map(|i| from + i)
    };
    let Some(star) = find(0, b"*") else { return Parsed::Error };
    let Some(nl) = find(star, b"\r\n") else { return Parsed::More };
    let argc = atoi(&buf[star + 1..nl]);
    if argc <= 0 || argc > 1024 {
        return Parsed::Error;
    }
    let mut p = nl + 2;
    let mut argv = Vec::with_capacity(argc as usize);
    while (argv.len() as i64) < argc {
        if p >= buf.len() || buf[p] == 0 {
            return Parsed::More;
        }
        if buf[p] != b'$' {
            return Parsed::Error;
        }
        let Some(nl) = find(p, b"\r\n") else { return Parsed::More };
        let len = atoi(&buf[p + 1..nl]);
        if len <= 0 || len > 1024 {
            return Parsed::Error;
        }
        let start = nl + 2;
        if start + len as usize + 2 > buf.len() {
            return Parsed::More;
        }
        argv.push(buf[start..start + len as usize].to_vec());
        p = start + len as usize;
        if &buf[p..p + 2] != b"\r\n" {
            return Parsed::Error;
        }
        p += 2;
    }
    Parsed::Command(argv)
}

/// How the debugging client left the prompt.
enum Leave {
    /// step / continue.
    Resume,
    /// It closed the connection.
    Gone,
    /// An error to raise was pushed (abort, a protocol error).
    Raise,
}

const HELP: &[&str] = &[
    "Redis Lua debugger help:",
    "[h]elp               Show this help.",
    "[s]tep               Run current line and stop again.",
    "[n]ext               Alias for step.",
    "[c]ontinue           Run till next breakpoint.",
    "[l]ist               List source code around current line.",
    "[l]ist [line]        List source code around [line].",
    "                     line = 0 means: current position.",
    "[l]ist [line] [ctx]  In this form [ctx] specifies how many lines",
    "                     to show before/after [line].",
    "[w]hole              List all source code. Alias for 'list 1 1000000'.",
    "[p]rint              Show all the local variables.",
    "[p]rint <var>        Show the value of the specified variable.",
    "                     Can also show global vars KEYS and ARGV.",
    "[b]reak              Show all breakpoints.",
    "[b]reak <line>       Add a breakpoint to the specified line.",
    "[b]reak -<line>      Remove breakpoint from the specified line.",
    "[b]reak 0            Remove all breakpoints.",
    "[t]race              Show a backtrace.",
    "[e]val <code>        Execute some Lua code (in a different callframe).",
    "[r]edis <cmd>        Execute a Redis command.",
    "[m]axlen [len]       Trim logged Redis replies and Lua var dumps to len.",
    "                     Specifying zero as <len> means unlimited.",
    "[a]bort              Stop the execution of the script. In sync",
    "                     mode dataset changes will be retained.",
    "",
    "Debugger functions you can call from Lua scripts:",
    "redis.debug()        Produce logs in the debugger console.",
    "redis.breakpoint()   Stop execution like if there was a breakpoint in the",
    "                     next line of code.",
];

/// `ldbRepl`: debugger commands until one resumes the script.
unsafe fn repl(l: *mut lua_State) -> Leave {
    unsafe {
        loop {
            let argv = loop {
                let Some(d) = ldb(l) else { return Leave::Gone };
                match parse_command(&d.cbuf) {
                    Parsed::Command(argv) => break argv,
                    Parsed::Error => {
                        push_error(l, b"protocol error");
                        return Leave::Raise;
                    }
                    Parsed::More => {
                        let mut buf = [0u8; 1024];
                        // SAFETY: see `IoPtr`.
                        let n = d.io.as_ref().map_or(0, |io| (*io.0).read(&mut buf));
                        if n == 0 {
                            // Let the script run on without anyone to stop for.
                            d.step = false;
                            d.bp.clear();
                            return Leave::Gone;
                        }
                        d.cbuf.extend_from_slice(&buf[..n]);
                        if d.cbuf.len() > 1 << 20 {
                            d.cbuf.clear();
                            push_error(l, b"max client buffer reached");
                            return Leave::Raise;
                        }
                    }
                }
            };
            let Some(d) = ldb(l) else { return Leave::Gone };
            d.cbuf.clear();
            let is = |names: &[&str]| names.iter().any(|n| argv[0].eq_ignore_ascii_case(n.as_bytes()));
            if is(&["h", "help"]) {
                for line in HELP {
                    d.log(*line);
                }
            } else if is(&["s", "step", "n", "next"]) {
                d.step = true;
                return Leave::Resume;
            } else if is(&["c", "continue"]) {
                return Leave::Resume;
            } else if is(&["t", "trace"]) {
                trace(l);
            } else if is(&["m", "maxlen"]) {
                d.maxlen_cmd(&argv);
            } else if is(&["b", "break"]) {
                d.break_cmd(&argv);
            } else if is(&["e", "eval"]) {
                eval(l, &argv);
            } else if is(&["a", "abort"]) {
                push_error(l, b"script aborted for user request");
                return Leave::Raise;
            } else if argv.len() > 1 && is(&["r", "redis"]) {
                redis(l, &argv);
            } else if is(&["p", "print"]) {
                if argv.len() == 2 {
                    print_var(l, &argv[1]);
                } else {
                    print_all(l);
                }
            } else if is(&["l", "list"]) {
                let mut around = d.currentline;
                if argv.len() > 1 {
                    let n = atoi(&argv[1]);
                    if n > 0 {
                        around = n as i32;
                    }
                }
                let context = if argv.len() > 2 { atoi(&argv[2]) as i32 } else { 5 };
                d.list(around, context);
            } else if is(&["w", "whole"]) {
                d.list(1, 1_000_000);
            } else {
                d.log("<error> Unknown Redis Lua debugger command or wrong number of arguments.");
            }
            if let Some(d) = ldb(l) {
                d.send_logs();
            }
        }
    }
}

/// `luaLdbLineHook`: stop on stepping, a breakpoint, `redis.breakpoint()` or a script running
/// past the busy threshold, and take debugger commands there.
pub(crate) unsafe extern "C" fn line_hook(l: *mut lua_State, count: c_int, line: c_int, src: *const c_char) -> c_int {
    unsafe {
        catch_unwind(AssertUnwindSafe(|| {
            let Some(run) = run_ctx(l) else { return 0 };
            let threshold = match run.cx().eng.cfg.busy_reply_threshold {
                0 => 5000,
                t => t,
            };
            let started = run.start;
            let Some(d) = ldb(l) else { return 0 };
            d.currentline = line;
            let bp = d.is_bp(line) || d.luabp;
            if src.is_null() || !is_user_script(CStr::from_ptr(src).to_bytes()) {
                return 0;
            }
            let mut timeout = false;
            if count != 0 && !d.step && !bp {
                if started.elapsed().as_millis() as u64 >= threshold {
                    timeout = true;
                    d.step = true;
                } else {
                    return 0;
                }
            }
            if !(d.step || bp) {
                return 0;
            }
            let reason = if bp {
                if d.luabp { "redis.breakpoint() called" } else { "break point" }
            } else if timeout {
                "timeout reached, infinite loop?"
            } else {
                "step over"
            };
            d.step = false;
            d.luabp = false;
            d.log(format!("* Stopped at {line}, stop reason = {reason}"));
            d.log_source_line(line);
            d.send_logs();
            match repl(l) {
                Leave::Raise => return 1,
                Leave::Gone if timeout => {
                    // Nobody will ever resume it: end the script rather than block forever.
                    push_error(l, b"timeout during Lua debugging with client closing connection");
                    return 1;
                }
                Leave::Gone | Leave::Resume => {}
            }
            if let Some(run) = run_ctx(l) {
                run.start = Instant::now();
            }
            0
        }))
        .unwrap_or(0)
    }
}

/// `redis.debug()`: the arguments logged, comma separated.
pub(crate) unsafe fn debug_log(l: *mut lua_State) {
    unsafe {
        let Some(d) = ldb(l) else { return };
        if !d.active {
            return;
        }
        let argc = lua_gettop(l);
        let mut s = format!("<debug> line {}: ", d.currentline).into_bytes();
        for i in 1..=argc {
            cat_value(&mut s, l, i);
            if i != argc {
                s.extend_from_slice(b", ");
            }
        }
        if let Some(d) = ldb(l) {
            d.log(s);
        }
    }
}

/// `redis.breakpoint()`: stop at the next line when debugging.
pub(crate) unsafe fn breakpoint(l: *mut lua_State) -> bool {
    unsafe {
        match ldb(l) {
            Some(d) if d.active => {
                d.luabp = true;
                true
            }
            _ => false,
        }
    }
}

/// Log an error raised for the script (`luaPushErrorBuff`) while stepping.
pub(crate) unsafe fn log_error(l: *mut lua_State, e: &[u8]) {
    unsafe {
        if let Some(d) = ldb(l)
            && d.logging()
        {
            let mut s = b"<error> ".to_vec();
            s.extend_from_slice(e);
            d.log(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_redis() {
        assert!(matches!(parse_command(b""), Parsed::More));
        assert!(matches!(parse_command(b"PING\r\n"), Parsed::Error));
        assert!(matches!(parse_command(b"*1\r\n$0\r\n\r\n"), Parsed::Error));
        assert!(matches!(parse_command(b"*2\r\n$1\r\ns\r\n"), Parsed::More));
        match parse_command(b"junk*2\r\n$1\r\nb\r\n$2\r\n10\r\nextra") {
            Parsed::Command(a) => assert_eq!(a, vec![b"b".to_vec(), b"10".to_vec()]),
            _ => panic!(),
        }
    }

    #[test]
    fn replies_read_as_redis_shows_them() {
        let mut o = Vec::new();
        human(&mut o, b"*3\r\n:1\r\n$-1\r\n+OK\r\n", 0);
        assert_eq!(o, b"[1,NULL,\"+OK\"]");
        let mut o = Vec::new();
        human(&mut o, b"%1\r\n$1\r\na\r\n,1.5\r\n", 0);
        assert_eq!(o, b"{\"a\" => (double) 1.5}");
        let mut o = Vec::new();
        repr(&mut o, b"a\nb\"c\x01");
        assert_eq!(o, b"\"a\\nb\\\"c\\x01\"");
    }

    #[test]
    fn atoi_is_c() {
        assert_eq!(atoi(b" 12x"), 12);
        assert_eq!(atoi(b"-5"), -5);
        assert_eq!(atoi(b"abc"), 0);
    }
}
