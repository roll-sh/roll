//! String commands.

use super::*;
use crate::cmd;
use crate::decimal::{self, IncrError, Ld};
use crate::notify;
use crate::util::{fmt_i64, parse_i64};
use crate::value::{Kind, Str, Value};

pub static COMMANDS: &[Command] = &[
    cmd!("get", 2, READONLY | FAST, 1, 1, 1, get, String),
    cmd!("set", -3, WRITE | DENYOOM, 1, 1, 1, set, String),
    cmd!("setnx", 3, WRITE | DENYOOM | FAST, 1, 1, 1, setnx, String),
    cmd!("setex", 4, WRITE | DENYOOM, 1, 1, 1, setex, String),
    cmd!("psetex", 4, WRITE | DENYOOM, 1, 1, 1, psetex, String),
    cmd!("getset", 3, WRITE | DENYOOM | FAST, 1, 1, 1, getset, String),
    cmd!("getdel", 2, WRITE | FAST, 1, 1, 1, getdel, String),
    cmd!("getex", -2, WRITE | FAST, 1, 1, 1, getex, String),
    cmd!("mget", -2, READONLY | FAST, 1, -1, 1, mget, String),
    cmd!("mset", -3, WRITE | DENYOOM, 1, -1, 2, mset, String),
    cmd!("msetnx", -3, WRITE | DENYOOM, 1, -1, 2, msetnx, String),
    cmd!("msetex", -4, WRITE | DENYOOM, keys = msetex_keys, msetex, String),
    cmd!("append", 3, WRITE | DENYOOM | FAST, 1, 1, 1, append, String),
    cmd!("strlen", 2, READONLY | FAST, 1, 1, 1, strlen, String),
    cmd!("incr", 2, WRITE | DENYOOM | FAST, 1, 1, 1, incr, String),
    cmd!("decr", 2, WRITE | DENYOOM | FAST, 1, 1, 1, decr, String),
    cmd!("incrby", 3, WRITE | DENYOOM | FAST, 1, 1, 1, incrby, String),
    cmd!("decrby", 3, WRITE | DENYOOM | FAST, 1, 1, 1, decrby, String),
    cmd!("incrbyfloat", 3, WRITE | DENYOOM | FAST, 1, 1, 1, incrbyfloat, String),
    Command { since: "8.8.0", ..cmd!("increx", -2, WRITE | DENYOOM | FAST, 1, 1, 1, increx, String) },
    cmd!("getrange", 4, READONLY, 1, 1, 1, getrange, String),
    cmd!("substr", 4, READONLY, 1, 1, 1, getrange, String),
    cmd!("setrange", 4, WRITE | DENYOOM, 1, 1, 1, setrange, String),
    cmd!("lcs", -3, READONLY, 1, 2, 1, lcs, String),
    cmd!("digest", 2, READONLY | FAST, 1, 1, 1, digest, String),
    cmd!("delex", -2, WRITE | FAST, 1, 1, 1, delex, String),
];

const TOO_BIG: Error = Error::new("ERR string exceeds maximum allowed size (proto-max-bulk-len)");

fn b(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

fn int_bytes(i: i64) -> Bytes {
    Bytes::from(fmt_i64(i))
}

pub fn get(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(match cx.get_str(&args[1])? {
        Some(s) => Reply::bulk(s),
        None => Reply::Nil,
    })
}

/// Expiration options shared by SET, GETEX and MSETEX.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Expiry {
    None,
    KeepTtl,
    Persist,
    /// Absolute Unix ms.
    At(i64),
}

/// Parse an EX/PX/EXAT/PXAT value into absolute ms (`getExpireMillisecondsOrReply`).
pub fn expire_value(opt: &[u8], v: &[u8], now: i64, cmd: &str) -> Result<i64, Error> {
    let n = int(v)?;
    let invalid = || Error::msg(format!("ERR invalid expire time in '{cmd}' command"));
    let secs = eq(opt, "ex") || eq(opt, "exat");
    if n <= 0 || (secs && n > i64::MAX / 1000) {
        return Err(invalid());
    }
    let mut ms = if secs { n * 1000 } else { n };
    if eq(opt, "ex") || eq(opt, "px") {
        ms = ms.checked_add(now).ok_or_else(invalid)?;
    }
    Ok(ms)
}

fn is_expire_opt(a: &[u8]) -> bool {
    eq(a, "ex") || eq(a, "px") || eq(a, "exat") || eq(a, "pxat")
}

#[derive(Clone, Copy, PartialEq)]
enum Cond<'a> {
    Any,
    Nx,
    Xx,
    IfEq(&'a [u8]),
    IfNe(&'a [u8]),
    IfDeq(&'a [u8]),
    IfDne(&'a [u8]),
}

fn xxh3_hex(v: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(v))
}

fn cond_holds(cond: Cond, cur: Option<&[u8]>) -> bool {
    match cond {
        Cond::Any => true,
        Cond::Nx => cur.is_none(),
        Cond::Xx => cur.is_some(),
        Cond::IfEq(v) => cur == Some(v),
        Cond::IfNe(v) => cur.is_some_and(|c| c != v),
        Cond::IfDeq(d) => cur.is_some_and(|c| xxh3_hex(c).as_bytes().eq_ignore_ascii_case(d)),
        Cond::IfDne(d) => cur.is_some_and(|c| !xxh3_hex(c).as_bytes().eq_ignore_ascii_case(d)),
    }
}

/// SET key value [NX | XX | IFEQ v | IFNE v | IFDEQ d | IFDNE d] [GET]
///     [EX s | PX ms | EXAT ts | PXAT ms | KEEPTTL]
pub fn set(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let o = parse_ext(args, 3, ExtFor::Set)?;
    let cond = o.cond;
    let get = o.get;
    let exp = o.expiry(cx.now, "set")?;
    let key = &args[1];
    // GET replies first (`getGenericCommand`): a WRONGTYPE ends the command there.
    let current: Option<Str> = if get { cx.get_str(key)?.cloned() } else { None };
    let exists = if get { current.is_some() } else { cx.peek(key).is_some() };
    let old_reply = |current: &Option<Str>| if get { Reply::opt_bulk(current.as_deref()) } else { Reply::Nil };
    if (cond == Cond::Nx && exists) || (matches!(cond, Cond::Xx | Cond::IfEq(_) | Cond::IfDeq(_)) && !exists) {
        return Ok(old_reply(&current));
    }
    // IFNE / IFDNE hold for a missing key; IF* compare an existing one (a string).
    if exists && matches!(cond, Cond::IfEq(_) | Cond::IfNe(_) | Cond::IfDeq(_) | Cond::IfDne(_)) {
        let cur = match &current {
            Some(c) => c.clone(),
            None => cx.get_str(key)?.cloned().unwrap_or_default(),
        };
        if let Cond::IfDeq(d) | Cond::IfDne(d) = cond
            && d.len() != 16
        {
            // After GET's reply Redis still adds the error: two replies.
            return if get { Ok(Reply::Seq(vec![old_reply(&current), Reply::Error(DIGEST_LEN.0.to_string())])) } else { Err(DIGEST_LEN) };
        }
        if !cond_holds(cond, Some(&cur)) {
            return Ok(old_reply(&current));
        }
    }
    write_string(cx, key, &args[2], exp);
    Ok(if get { old_reply(&current) } else { Reply::Ok })
}

const DIGEST_LEN: Error = Error::new("ERR must be exactly 16 hexadecimal characters");

#[derive(Clone, Copy, PartialEq)]
enum ExtFor {
    Set,
    Get,
    Msetex,
}

/// SET / GETEX / MSETEX options, as `parseExtendedStringArgumentsOrReply` reads them: one
/// condition (repeating the same one is fine, the last value counts), one kind of expiry (the
/// same option again replaces the time), values checked only later.
struct Ext<'a> {
    cond: Cond<'a>,
    get: bool,
    keepttl: bool,
    persist: bool,
    /// The option (`ex`, `px`, `exat`, `pxat`) and its value.
    expire: Option<(&'a [u8], &'a [u8])>,
}

impl Ext<'_> {
    /// The expiry, with the time checked (`getExpireMillisecondsOrReply`).
    fn expiry(&self, now: i64, cmd: &str) -> Result<Expiry, Error> {
        Ok(match self.expire {
            Some((opt, v)) => Expiry::At(expire_value(opt, v, now, cmd)?),
            None if self.keepttl => Expiry::KeepTtl,
            None if self.persist => Expiry::Persist,
            None => Expiry::None,
        })
    }
}

fn parse_ext(args: &[Bytes], start: usize, what: ExtFor) -> Result<Ext<'_>, Error> {
    let mut o = Ext { cond: Cond::Any, get: false, keepttl: false, persist: false, expire: None };
    let set_like = what != ExtFor::Get;
    let mut i = start;
    while i < args.len() {
        let a = &args[i][..];
        let next = args.get(i + 1).map(|v| &v[..]);
        let exp_kind = o.expire.map(|(k, _)| k);
        let other_exp = |k: &str| exp_kind.is_some_and(|e| !eq(e, k));
        let cond_ok = |c: &Cond| o.cond == Cond::Any || std::mem::discriminant(&o.cond) == std::mem::discriminant(c);
        if eq(a, "nx") && set_like && cond_ok(&Cond::Nx) {
            o.cond = Cond::Nx;
        } else if eq(a, "xx") && set_like && cond_ok(&Cond::Xx) {
            o.cond = Cond::Xx;
        } else if eq(a, "get") && what == ExtFor::Set {
            o.get = true;
        } else if eq(a, "keepttl") && set_like && !o.persist && exp_kind.is_none() {
            o.keepttl = true;
        } else if eq(a, "persist") && what == ExtFor::Get && exp_kind.is_none() && !o.keepttl {
            o.persist = true;
        } else if let Some(v) = next.filter(|_| is_expire_opt(a) && !o.keepttl && !o.persist && !other_exp(&String::from_utf8_lossy(a))) {
            o.expire = Some((a, v));
            i += 1;
        } else if let Some(v) = next.filter(|_| what == ExtFor::Set && (eq(a, "ifeq") || eq(a, "ifne") || eq(a, "ifdeq") || eq(a, "ifdne"))) {
            let c = if eq(a, "ifeq") {
                Cond::IfEq(v)
            } else if eq(a, "ifne") {
                Cond::IfNe(v)
            } else if eq(a, "ifdeq") {
                Cond::IfDeq(v)
            } else {
                Cond::IfDne(v)
            };
            if !cond_ok(&c) {
                return Err(SYNTAX);
            }
            o.cond = c;
            i += 1;
        } else {
            return Err(SYNTAX);
        }
        i += 1;
    }
    Ok(o)
}

/// Store a string with SET semantics (notifications, propagation with absolute expiry).
fn write_string(cx: &mut Cx, key: &Bytes, value: &Bytes, exp: Expiry) {
    match exp {
        Expiry::At(at) if at <= cx.now => {
            // Already expired: an existing key is deleted (propagated as a DEL) and nothing is
            // stored; stats count it as set and expired.
            if cx.delete(key) {
                cx.notify(notify::GENERIC, "del", key);
                let del = if cx.cfg().lazyfree_lazy_server_del { "UNLINK" } else { "DEL" };
                cx.propagate_as(vec![b(del), key.clone()]);
            } else {
                cx.changes(0);
            }
            cx.eng.stats.expired_keys += 1;
        }
        Expiry::At(at) => {
            cx.set_with_expire(key, Value::str(value), at);
            cx.notify(notify::STRING, "set", key);
            cx.notify(notify::GENERIC, "expire", key);
            cx.propagate_as(vec![b("SET"), key.clone(), value.clone(), b("PXAT"), int_bytes(at)]);
        }
        Expiry::KeepTtl => {
            cx.set(key, Value::str(value), true);
            cx.notify(notify::STRING, "set", key);
            cx.propagate_as(vec![b("SET"), key.clone(), value.clone(), b("KEEPTTL")]);
        }
        _ => {
            cx.set(key, Value::str(value), false);
            cx.notify(notify::STRING, "set", key);
            cx.propagate_as(vec![b("SET"), key.clone(), value.clone()]);
        }
    }
}

pub fn setnx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if cx.peek(&args[1]).is_some() {
        return Ok(Reply::Int(0));
    }
    write_string(cx, &args[1], &args[2], Expiry::None);
    Ok(Reply::Int(1))
}

fn setex_generic(cx: &mut Cx, args: &[Bytes], unit: &str, name: &str) -> CmdResult {
    let at = expire_value(unit.as_bytes(), &args[2], cx.now, name)?;
    write_string(cx, &args[1], &args[3], Expiry::At(at));
    Ok(Reply::Ok)
}

pub fn setex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    setex_generic(cx, args, "ex", "setex")
}

pub fn psetex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    setex_generic(cx, args, "px", "psetex")
}

pub fn getset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let old = cx.get_str(&args[1])?.cloned();
    write_string(cx, &args[1], &args[2], Expiry::None);
    Ok(Reply::opt_bulk(old))
}

pub fn getdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let Some(v) = cx.get_str(&args[1])?.cloned() else { return Ok(Reply::Nil) };
    cx.delete(&args[1]);
    cx.notify(notify::GENERIC, "del", &args[1]);
    cx.propagate_as(vec![b("DEL"), args[1].clone()]);
    Ok(Reply::bulk(v))
}

/// GETEX key [EX s | PX ms | EXAT ts | PXAT ms | PERSIST]
pub fn getex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let o = parse_ext(args, 2, ExtFor::Get)?;
    let key = &args[1];
    let Some(v) = cx.get_str(key)?.cloned() else { return Ok(Reply::Nil) };
    // The time is checked once the key is known to be a string.
    let exp = o.expiry(cx.now, "getex")?;
    match exp {
        Expiry::At(at) if at <= cx.now => {
            cx.delete(key);
            cx.notify(notify::GENERIC, "del", key);
            cx.propagate_as(vec![b("DEL"), key.clone()]);
        }
        Expiry::At(at) => {
            cx.set_expire(key, at);
            cx.notify(notify::GENERIC, "expire", key);
            cx.propagate_as(vec![b("PEXPIREAT"), key.clone(), int_bytes(at)]);
        }
        Expiry::Persist
            if cx.expire_of(key).unwrap_or(0) != 0 => {
                cx.set_expire(key, 0);
                cx.notify(notify::GENERIC, "persist", key);
                cx.propagate_as(vec![b("PERSIST"), key.clone()]);
            }
        _ => {}
    }
    Ok(Reply::bulk(v))
}

pub fn mget(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut out = Vec::with_capacity(args.len() - 1);
    for k in &args[1..] {
        out.push(match cx.get(k) {
            Some(Value::Str(s)) => Reply::bulk(s),
            _ => Reply::Nil,
        });
    }
    Ok(Reply::Array(out))
}

pub fn mset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len().is_multiple_of(2) {
        return Err(arity_error("mset"));
    }
    for kv in args[1..].chunks(2) {
        cx.set(&kv[0], Value::str(&kv[1]), false);
        cx.notify(notify::STRING, "set", &kv[0]);
    }
    Ok(Reply::Ok)
}

pub fn msetnx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len().is_multiple_of(2) {
        return Err(arity_error("msetnx"));
    }
    for kv in args[1..].chunks(2) {
        if cx.peek(&kv[0]).is_some() {
            return Ok(Reply::Int(0));
        }
    }
    for kv in args[1..].chunks(2) {
        cx.set(&kv[0], Value::str(&kv[1]), false);
        cx.notify(notify::STRING, "set", &kv[0]);
    }
    Ok(Reply::Int(1))
}

fn msetex_keys(args: &[Bytes]) -> Vec<usize> {
    let Some(n) = args.get(1).and_then(|a| parse_i64(a)) else { return vec![] };
    (0..n.max(0) as usize).map(|i| 2 + 2 * i).filter(|&i| i < args.len()).collect()
}

/// MSETEX numkeys key value [key value …] [NX | XX] [EX s | PX ms | EXAT ts | PXAT ms | KEEPTTL]
pub fn msetex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let n = match parse_i64(&args[1]) {
        Some(n) if (1..=i32::MAX as i64).contains(&n) => n as usize,
        _ => return Err(Error::new("ERR invalid numkeys value")),
    };
    let data_end = 2 + 2 * n;
    if args.len() < data_end {
        return Err(Error::new("ERR wrong number of key-value pairs"));
    }
    let o = parse_ext(args, data_end, ExtFor::Msetex)?;
    let (nx, xx) = (o.cond == Cond::Nx, o.cond == Cond::Xx);
    // KEEPTTL with an expiry can't parse, so either applies alone.
    let exp = o.expiry(cx.now, "msetex")?;
    let pairs: Vec<(&Bytes, &Bytes)> = (0..n).map(|j| (&args[2 + 2 * j], &args[3 + 2 * j])).collect();
    if nx || xx {
        for (k, _) in &pairs {
            let exists = cx.peek(k).is_some();
            if (nx && exists) || (xx && !exists) {
                return Ok(Reply::Int(0));
            }
        }
    }
    let mut prop = vec![args[0].clone(), args[1].clone()];
    for (k, v) in &pairs {
        match exp {
            Expiry::At(at) => cx.set_with_expire(k, Value::str(v), at),
            Expiry::KeepTtl => cx.set(k, Value::str(v), true),
            _ => cx.set(k, Value::str(v), false),
        }
        // `expire` first, then `set` (msetexCommand).
        if matches!(exp, Expiry::At(_)) {
            cx.notify(notify::GENERIC, "expire", k);
        }
        cx.notify(notify::STRING, "set", k);
        prop.push((*k).clone());
        prop.push((*v).clone());
    }
    match exp {
        Expiry::At(at) => {
            prop.push(b("PXAT"));
            prop.push(int_bytes(at));
        }
        Expiry::KeepTtl => prop.push(b("KEEPTTL")),
        _ => {}
    }
    cx.propagate_as(prop);
    Ok(Reply::Int(1))
}

fn max_bulk(cx: &Cx) -> usize {
    cx.cfg().proto_max_bulk_len as usize
}

pub fn append(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let limit = max_bulk(cx);
    if let Some(cur) = cx.get_str(&args[1])?
        && cur.len() + args[2].len() > limit {
            return Err(TOO_BIG);
        }
    let existed = cx.peek(&args[1]).is_some();
    let mut e = cx.get_or_create(&args[1], Kind::String)?;
    let s = if existed {
        let to = e.str().len() + args[2].len();
        e.str_in_place(to)
    } else {
        e.str()
    };
    s.extend_from_slice(&args[2]);
    let len = s.len();
    e.notify(notify::STRING, "append");
    Ok(Reply::Int(len as i64))
}

pub fn strlen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.get_str(&args[1])?.map_or(0, |s| s.len()) as i64))
}

fn incr_generic(cx: &mut Cx, key: &Bytes, by: i64) -> CmdResult {
    let shared_ints = cx.cfg().shared_integers();
    // One lookup for an existing key (`lookupKeyWrite`, then the value changed through it).
    let found = cx.get_mut_kind(key, Kind::String)?;
    let Some(mut e) = found else {
        drop(found);
        return incr_store(cx.get_or_create(key, Kind::String)?, by, shared_ints);
    };
    match parse_i64(e.str()).ok_or(NOT_INT).and_then(|cur| cur.checked_add(by).ok_or(OVERFLOW)) {
        Ok(new) => incr_store(e, new, shared_ints),
        Err(err) => {
            e.unchanged();
            Err(err)
        }
    }
}

/// Store INCR's result: an int-encoded value changes in place; anything else is replaced.
fn incr_store(mut e: crate::cx::KeyMut, new: i64, shared_ints: bool) -> CmdResult {
    let in_place = e.encoding() == "int";
    let embedded = e.embedded();
    let s = e.str();
    s.clear();
    s.extend_from_slice(itoa::Buffer::new().format(new).as_bytes());
    if !in_place {
        e.str_replaced(embedded, crate::object::ENC_AUTO, shared_ints);
    }
    e.notify(notify::STRING, "incrby");
    Ok(Reply::Int(new))
}

pub fn incr(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    incr_generic(cx, &args[1], 1)
}

pub fn decr(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    incr_generic(cx, &args[1], -1)
}

pub fn incrby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let by = int(&args[2])?;
    incr_generic(cx, &args[1], by)
}

pub fn decrby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let by = int(&args[2])?;
    if by == i64::MIN {
        return Err(Error::new("ERR decrement would overflow"));
    }
    incr_generic(cx, &args[1], -by)
}

pub fn incrbyfloat(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let cur: Vec<u8> = cx.get_str(key)?.map_or(b"0".to_vec(), |s| s.to_vec());
    if !decimal::valid(&args[2]) {
        return Err(NOT_FLOAT);
    }
    let shared_ints = cx.cfg().shared_integers();
    let new = decimal::incr_by_float(&cur, &args[2]).map_err(|e| match e {
        IncrError::NotFloat => NOT_FLOAT,
        IncrError::NanOrInf => Error::new("ERR increment would produce NaN or Infinity"),
    })?;
    let mut e = cx.get_or_create(key, Kind::String)?;
    let embedded = e.embedded();
    let s = e.str();
    s.clear();
    s.extend_from_slice(new.as_bytes());
    e.str_replaced(embedded, crate::object::ENC_STR, shared_ints);
    e.notify(notify::STRING, "incrbyfloat");
    drop(e);
    let nb = Bytes::from(new);
    cx.propagate_as(vec![b("SET"), key.clone(), nb.clone(), b("KEEPTTL")]);
    Ok(Reply::Bulk(nb))
}

enum Incr {
    Int(i64),
    Float(Ld),
}

enum IncrExpire {
    Keep,
    Persist,
    At(i64),
}

/// INCREX key [BYINT n | BYFLOAT f] [LBOUND l] [UBOUND u] [SATURATE]
/// [EX s | PX ms | EXAT s | PXAT ms | PERSIST] [ENX]
pub fn increx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let msg = |m: &str| Error::msg(format!("ERR {m}"));
    let (mut by, mut lb, mut ub): (Option<Incr>, Option<&Bytes>, Option<&Bytes>) = (None, None, None);
    let (mut saturate, mut enx) = (false, false);
    let mut expire: Option<(&Bytes, &Bytes)> = None;
    let mut persist = false;
    let mut j = 2;
    while j < args.len() {
        let next = args.get(j + 1);
        let a = &args[j];
        let expiry_set = expire.is_some() || persist;
        if let Some(n) = next
            && eq(a, "byint")
            && by.is_none()
        {
            by = Some(Incr::Int(parse_i64(n).ok_or_else(|| msg("Increment is not an integer or out of range"))?));
            j += 1;
        } else if let Some(n) = next
            && eq(a, "byfloat")
            && by.is_none()
        {
            let v = Ld::parse(n).ok_or_else(|| msg("Increment is not a valid float"))?;
            if v.is_inf() {
                return Err(msg("BYFLOAT increment cannot be Infinity"));
            }
            by = Some(Incr::Float(v));
            j += 1;
        } else if eq(a, "lbound") && next.is_some() && lb.is_none() {
            lb = next;
            j += 1;
        } else if eq(a, "ubound") && next.is_some() && ub.is_none() {
            ub = next;
            j += 1;
        } else if eq(a, "saturate") && !saturate {
            saturate = true;
        } else if eq(a, "enx") && !enx && !persist {
            enx = true;
        } else if eq(a, "persist") && !expiry_set && !enx {
            persist = true;
        } else if let Some(n) = next
            && is_expire_opt(a)
            && !expiry_set
        {
            expire = Some((a, n));
            j += 1;
        } else {
            return Err(SYNTAX);
        }
        j += 1;
    }
    let by = by.unwrap_or(Incr::Int(1));
    // Bounds take the increment's type, known only after the loop.
    enum Bounds {
        Int(i64, i64),
        Float(Ld, Ld),
    }
    let bounds = match &by {
        Incr::Float(_) => {
            let lo = lb.map(|v| Ld::parse(v).ok_or_else(|| msg("LBOUND is not a valid float"))).transpose()?.unwrap_or_else(Ld::min);
            let hi = ub.map(|v| Ld::parse(v).ok_or_else(|| msg("UBOUND is not a valid float"))).transpose()?.unwrap_or_else(Ld::max);
            if lo.compare(&hi) == std::cmp::Ordering::Greater {
                return Err(msg("LBOUND can't be greater than UBOUND"));
            }
            Bounds::Float(lo, hi)
        }
        Incr::Int(_) => {
            let p = |v: Option<&Bytes>, m: &str, d: i64| v.map_or(Ok(d), |v| parse_i64(v).ok_or_else(|| msg(m)));
            let lo = p(lb, "LBOUND is not an integer or out of range", i64::MIN)?;
            let hi = p(ub, "UBOUND is not an integer or out of range", i64::MAX)?;
            if lo > hi {
                return Err(msg("LBOUND can't be greater than UBOUND"));
            }
            Bounds::Int(lo, hi)
        }
    };
    if enx && expire.is_none() && !persist {
        return Err(msg("ENX flag requires an expiration"));
    }
    let expire_at = expire.map(|(o, v)| expire_value(o, v, cx.now, "increx")).transpose()?;

    let key = &args[1];
    let cur = cx.get_str(key)?.map(|s| s.to_vec());
    let exists = cur.is_some();
    let resp3 = cx.resp() == 3;
    let human = |v: &Ld| if resp3 { Reply::Raw(Bytes::from(format!(",{}\r\n", v.human()))) } else { Reply::bulk(v.human()) };
    let (new, reply) = match (by, bounds) {
        (Incr::Float(incr), Bounds::Float(lo, hi)) => {
            let old = match &cur {
                Some(s) => Ld::parse(s).ok_or(NOT_FLOAT)?,
                None => Ld::ZERO,
            };
            if old.is_inf() {
                return Err(msg("value cannot be Infinity"));
            }
            let mut v = old.add(&incr);
            let overflow = v.is_inf();
            if overflow || v.compare(&hi).is_gt() || v.compare(&lo).is_lt() {
                if !saturate {
                    return Ok(Reply::Array(vec![human(&old), human(&Ld::ZERO)]));
                }
                v = if overflow { if incr.is_neg() { lo } else { hi } } else if v.compare(&hi).is_gt() { hi } else { lo };
            }
            let delta = v.sub(&old);
            if delta.is_inf() {
                return Err(msg("applied increment would be Infinity"));
            }
            (v.human(), Reply::Array(vec![human(&v), human(&delta)]))
        }
        (Incr::Int(incr), Bounds::Int(lo, hi)) => {
            let old = match &cur {
                Some(s) => parse_i64(s).ok_or(NOT_INT)?,
                None => 0,
            };
            let (mut v, overflow) = old.overflowing_add(incr);
            if overflow || v > hi || v < lo {
                if !saturate {
                    return Ok(Reply::Array(vec![Reply::Int(old), Reply::Int(0)]));
                }
                v = if overflow { if incr >= 0 { hi } else { lo } } else if v > hi { hi } else { lo };
            }
            let delta = v.checked_sub(old).ok_or_else(|| msg("applied increment would overflow"))?;
            (v.to_string(), Reply::Array(vec![Reply::Int(v), Reply::Int(delta)]))
        }
        _ => unreachable!("bounds follow the increment type"),
    };
    let float = matches!(reply, Reply::Array(ref r) if !matches!(r[0], Reply::Int(_)));
    let had_ttl = cx.expire_of(key).is_some_and(|at| at != 0);
    let new_ttl = expire_at.filter(|_| !enx || !had_ttl);
    if let Some(at) = new_ttl
        && at <= cx.now
    {
        // Already expired: the key goes away (replicated as DEL).
        if exists && cx.delete(key) {
            cx.notify(notify::GENERIC, "del", key);
            cx.propagate_as(vec![b("DEL"), key.clone()]);
        } else {
            cx.no_propagate();
        }
        return Ok(reply);
    }
    let what = if persist {
        IncrExpire::Persist
    } else if let Some(at) = new_ttl {
        IncrExpire::At(at)
    } else {
        IncrExpire::Keep
    };
    if float {
        // createStringObjectFromLongDouble: a string object, never int-encoded.
        cx.set_encoded(key, Value::str(new.as_bytes()), true, false, crate::object::ENC_STR);
    } else {
        cx.replace(key, Value::str(new.as_bytes()), true);
    }
    cx.changes(1);
    let nb = Bytes::from(new);
    cx.notify(notify::STRING, if float { "incrbyfloat" } else { "incrby" }, key);
    match what {
        IncrExpire::Persist => {
            if had_ttl {
                cx.set_expire(key, 0);
                cx.notify(notify::GENERIC, "persist", key);
            }
            cx.propagate_as(vec![b("SET"), key.clone(), nb]);
        }
        IncrExpire::At(at) => {
            cx.set_expire(key, at);
            cx.notify(notify::GENERIC, "expire", key);
            cx.propagate_as(vec![b("SET"), key.clone(), nb, b("PXAT"), Bytes::from(at.to_string())]);
        }
        IncrExpire::Keep => cx.propagate_as(vec![b("SET"), key.clone(), nb, b("KEEPTTL")]),
    }
    Ok(reply)
}

pub fn getrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut start = int(&args[2])?;
    let mut end = int(&args[3])?;
    let Some(s) = cx.get_str(&args[1])? else { return Ok(Reply::bulk("")) };
    let len = s.len() as i64;
    if start < 0 && end < 0 && start > end {
        return Ok(Reply::bulk(""));
    }
    if start < 0 {
        start += len;
    }
    if end < 0 {
        end += len;
    }
    start = start.max(0);
    end = end.max(0);
    if end >= len {
        end = len - 1;
    }
    if start > end || len == 0 {
        return Ok(Reply::bulk(""));
    }
    Ok(Reply::bulk(&s[start as usize..=end as usize]))
}

pub fn setrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let offset = int(&args[2])?;
    if offset < 0 {
        return Err(Error::new("ERR offset is out of range"));
    }
    let value = &args[3];
    let offset = offset as usize;
    let cur_len = cx.get_str(&args[1])?.map(|s| s.len());
    if value.is_empty() {
        return Ok(Reply::Int(cur_len.unwrap_or(0) as i64));
    }
    if offset + value.len() > max_bulk(cx) {
        return Err(TOO_BIG);
    }
    let mut e = cx.get_or_create(&args[1], Kind::String)?;
    let s = if cur_len.is_some() { e.str_in_place(offset + value.len()) } else { e.str_new_raw() };
    if s.len() < offset + value.len() {
        s.resize(offset + value.len(), 0);
    }
    s[offset..offset + value.len()].copy_from_slice(value);
    let len = s.len();
    e.notify(notify::STRING, "setrange");
    Ok(Reply::Int(len as i64))
}

/// LCS key1 key2 [LEN] [IDX] [MINMATCHLEN len] [WITHMATCHLEN]
pub fn lcs(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    // The keys' types first, then the options (lcsCommand).
    let type_err = Error::new("ERR The specified keys must contain string values");
    let a = match cx.get(&args[1]) {
        None => Vec::new(),
        Some(Value::Str(s)) => s.to_vec(),
        Some(_) => return Err(type_err),
    };
    let bv = match cx.get(&args[2]) {
        None => Vec::new(),
        Some(Value::Str(s)) => s.to_vec(),
        Some(_) => return Err(type_err),
    };
    let (mut want_len, mut want_idx, mut with_len) = (false, false, false);
    let mut min_len: i64 = 0;
    let mut i = 3;
    while i < args.len() {
        let a = &args[i];
        if eq(a, "len") {
            want_len = true;
        } else if eq(a, "idx") {
            want_idx = true;
        } else if eq(a, "withmatchlen") {
            with_len = true;
        } else if eq(a, "minmatchlen") && i + 1 < args.len() {
            min_len = int(&args[i + 1])?.max(0);
            i += 1;
        } else {
            return Err(SYNTAX);
        }
        i += 1;
    }
    if want_len && want_idx {
        return Err(Error::new("ERR If you want both the length and indexes, please just use IDX."));
    }
    let (n, m) = (a.len(), bv.len());
    // The table of u32 cells against proto-max-bulk-len.
    if ((n + 1) as u64).saturating_mul((m + 1) as u64).saturating_mul(4) > cx.cfg().proto_max_bulk_len {
        return Err(Error::new("ERR Insufficient memory, transient memory for LCS exceeds proto-max-bulk-len"));
    }
    // dp[i][j] = LCS length of a[..i], b[..j]
    let w = m + 1;
    let mut dp = vec![0u32; (n + 1) * w];
    for i in 1..=n {
        for j in 1..=m {
            dp[i * w + j] = if a[i - 1] == bv[j - 1] {
                dp[(i - 1) * w + j - 1] + 1
            } else {
                dp[(i - 1) * w + j].max(dp[i * w + j - 1])
            };
        }
    }
    let total = dp[n * w + m] as usize;
    if want_len {
        return Ok(Reply::Int(total as i64));
    }
    // Walk back from the end, collecting the LCS and (for IDX) contiguous match ranges.
    let mut out = vec![0u8; total];
    let mut idx = total;
    let (mut i, mut j) = (n, m);
    let mut matches = Vec::new();
    let mut range: Option<(usize, usize, usize, usize)> = None; // a_start,a_end,b_start,b_end
    let flush = |range: &mut Option<(usize, usize, usize, usize)>, matches: &mut Vec<Reply>| {
        if let Some((a1, a2, b1, b2)) = range.take() {
            let len = a2 - a1 + 1;
            if len as i64 >= min_len {
                let mut m = vec![
                    Reply::Array(vec![Reply::Int(a1 as i64), Reply::Int(a2 as i64)]),
                    Reply::Array(vec![Reply::Int(b1 as i64), Reply::Int(b2 as i64)]),
                ];
                if with_len {
                    m.push(Reply::Int(len as i64));
                }
                matches.push(Reply::Array(m));
            }
        }
    };
    while i > 0 && j > 0 {
        if a[i - 1] == bv[j - 1] {
            idx -= 1;
            out[idx] = a[i - 1];
            let (ai, bi) = (i - 1, j - 1);
            match &mut range {
                Some((a1, _, b1, _)) if *a1 == ai + 1 && *b1 == bi + 1 => {
                    *a1 = ai;
                    *b1 = bi;
                }
                _ => {
                    flush(&mut range, &mut matches);
                    range = Some((ai, ai, bi, bi));
                }
            }
            i -= 1;
            j -= 1;
        } else if dp[(i - 1) * w + j] > dp[i * w + j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    flush(&mut range, &mut matches);
    if want_idx {
        return Ok(Reply::Map(vec![
            (Reply::bulk("matches"), Reply::Array(matches)),
            (Reply::bulk("len"), Reply::Int(total as i64)),
        ]));
    }
    Ok(Reply::bulk(out))
}

pub fn digest(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(match cx.get_str(&args[1])? {
        Some(s) => Reply::bulk_str(xxh3_hex(s)),
        None => Reply::Nil,
    })
}

/// DELEX key [IFEQ v | IFNE v | IFDEQ d | IFDNE d]
pub fn delex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    match args.len() {
        2 => {}
        4 => {
            // A missing key first, then its type, then the condition (`delexCommand`).
            let cur = match cx.get(key) {
                None => return Ok(Reply::Int(0)),
                Some(Value::Str(s)) => s.clone(),
                Some(_) => return Err(Error::new("ERR Key should be of string type if conditions are specified")),
            };
            let v = &args[3][..];
            let a = &args[2];
            let cond = if eq(a, "ifeq") {
                Cond::IfEq(v)
            } else if eq(a, "ifne") {
                Cond::IfNe(v)
            } else if eq(a, "ifdeq") {
                Cond::IfDeq(v)
            } else if eq(a, "ifdne") {
                Cond::IfDne(v)
            } else {
                return Err(Error::new("ERR Invalid condition. Use IFEQ, IFNE, IFDEQ, or IFDNE"));
            };
            if matches!(cond, Cond::IfDeq(_) | Cond::IfDne(_)) && v.len() != 16 {
                return Err(DIGEST_LEN);
            }
            if !cond_holds(cond, Some(&cur)) {
                return Ok(Reply::Int(0));
            }
        }
        _ => return Err(arity_error("delex")),
    }
    if cx.delete(key) {
        cx.notify(notify::GENERIC, "del", key);
        cx.propagate_as(vec![b("DEL"), key.clone()]);
        Ok(Reply::Int(1))
    } else {
        Ok(Reply::Int(0))
    }
}
