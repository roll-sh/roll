//! Hash commands, including hash field expiration (HEXPIRE & co, HGETEX, HSETEX, HGETDEL).
//!
//! Field expiry follows Redis' model (`t_hash.c`):
//!
//! - A hash with field TTLs is registered in its database's `subexpires` index with its next field
//!   expiry ([`Hash::subexpiry`]). Like Redis the registration is only refreshed at specific
//!   points (`hashTypeSetExDone`, HDEL/HGETDEL of the last volatile field, active expiry), so it
//!   can lag behind; INFO keyspace `subexpiry` counts registered hashes.
//! - Active expiry deletes the due fields of registered hashes: each is propagated as
//!   `HDEL key field`, then one `hexpired` event, and `del` if the hash became empty. Redis runs it
//!   from its cron; here it also runs when a due hash is looked up (`Engine::expire_if_needed`),
//!   so commands see what they would see on a Redis whose cron already ran.
//! - Commands that access single fields delete an expired field lazily (`hashTypeGetValue`),
//!   others (HGETALL, HSCAN…) just skip expired fields and HLEN still counts them.
//! - Replicas never delete (they wait for the primary's HDELs) but hide expired fields from
//!   clients; replayed effects see every field.

use super::keys::{ScanOpts, parse_cursor, scan_reply};
use super::*;
use crate::cmd;
use crate::decimal::{self, IncrError};
use crate::engine::{Effect, Engine, Mode};
use crate::notify;
use crate::types::Hash;
use crate::types::hash::{Limits, expired};
use crate::util::{fmt_i64, parse_i64};
use crate::value::{Entry, Kind, Str, Value};
use std::time::Instant;

pub static COMMANDS: &[Command] = &[
    cmd!("hset", -4, WRITE | DENYOOM | FAST, 1, 1, 1, hset, Hash),
    cmd!("hmset", -4, WRITE | DENYOOM | FAST, 1, 1, 1, hset, Hash),
    cmd!("hsetnx", 4, WRITE | DENYOOM | FAST, 1, 1, 1, hsetnx, Hash),
    cmd!("hget", 3, READONLY | FAST, 1, 1, 1, hget, Hash),
    cmd!("hmget", -3, READONLY | FAST, 1, 1, 1, hmget, Hash),
    cmd!("hgetall", 2, READONLY, 1, 1, 1, hgetall, Hash),
    cmd!("hkeys", 2, READONLY, 1, 1, 1, hkeys, Hash),
    cmd!("hvals", 2, READONLY, 1, 1, 1, hvals, Hash),
    cmd!("hdel", -3, WRITE | FAST, 1, 1, 1, hdel, Hash),
    cmd!("hlen", 2, READONLY | FAST, 1, 1, 1, hlen, Hash),
    cmd!("hstrlen", 3, READONLY | FAST, 1, 1, 1, hstrlen, Hash),
    cmd!("hexists", 3, READONLY | FAST, 1, 1, 1, hexists, Hash),
    cmd!("hincrby", 4, WRITE | DENYOOM | FAST, 1, 1, 1, hincrby, Hash),
    cmd!("hincrbyfloat", 4, WRITE | DENYOOM | FAST, 1, 1, 1, hincrbyfloat, Hash),
    cmd!("hrandfield", -2, READONLY, 1, 1, 1, hrandfield, Hash),
    cmd!("hscan", -3, READONLY, 1, 1, 1, hscan, Hash),
    cmd!("hexpire", -6, WRITE | FAST, 1, 1, 1, hexpire, Hash),
    cmd!("hpexpire", -6, WRITE | FAST, 1, 1, 1, hpexpire, Hash),
    cmd!("hexpireat", -6, WRITE | FAST, 1, 1, 1, hexpireat, Hash),
    cmd!("hpexpireat", -6, WRITE | FAST, 1, 1, 1, hpexpireat, Hash),
    cmd!("httl", -5, READONLY | FAST, 1, 1, 1, httl, Hash),
    cmd!("hpttl", -5, READONLY | FAST, 1, 1, 1, hpttl, Hash),
    cmd!("hexpiretime", -5, READONLY | FAST, 1, 1, 1, hexpiretime, Hash),
    cmd!("hpexpiretime", -5, READONLY | FAST, 1, 1, 1, hpexpiretime, Hash),
    cmd!("hpersist", -5, WRITE | FAST, 1, 1, 1, hpersist, Hash),
    cmd!("hgetdel", -5, WRITE | FAST, 1, 1, 1, hgetdel, Hash),
    cmd!("hgetex", -5, WRITE | FAST, 1, 1, 1, hgetex, Hash),
    cmd!("hsetex", -6, WRITE | DENYOOM | FAST, 1, 1, 1, hsetex, Hash),
];

/// `EB_EXPIRE_TIME_INVALID`: how Redis represents "no expiry" in min-expiry arithmetic.
const NO_EXPIRY: i64 = 1 << 48;
/// `HFE_MAX_ABS_TIME_MSEC`: the largest field expiry accepted.
const MAX_EXPIRE_MS: i64 = ((1 << 48) - 1) >> 2;
/// `HASH_NEW_EXPIRE_DIFF_THRESHOLD`: smaller changes of a hash's next expiry don't re-register it.
const REREGISTER_DIFF: i64 = 4000;

const FIELDS_POSITION: Error = Error::new("ERR Mandatory argument FIELDS is missing or not at the right position");
const NUMFIELDS_POSITIVE: Error = Error::new("ERR Number of fields must be a positive integer");
const NUMFIELDS_MATCH: Error = Error::new("ERR The `numfields` parameter must match the number of arguments");
const NOT_INT_VALUE: Error = Error::new("ERR hash value is not an integer");
const NOT_FLOAT_VALUE: Error = Error::new("ERR hash value is not a float");

// Per-field results of the expiration commands.
const NO_FIELD: i64 = -2;
const NO_TTL: i64 = -1;
const NO_CONDITION: i64 = 0;
const UPDATED: i64 = 1;
const DELETED: i64 = 2;

fn b(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

fn int_bytes(i: i64) -> Bytes {
    Bytes::from(fmt_i64(i))
}

// ---- context helpers ----

/// How field expiry applies to the running command.
#[derive(Clone, Copy)]
struct Clock {
    /// Fields with `0 < expire < now` are expired.
    now: i64,
    /// Expired fields may be deleted (a primary serving a client).
    delete: bool,
}

fn clock(cx: &Cx) -> Clock {
    if cx.mode == Mode::Replay {
        // The primary decides what expired; its HDELs follow.
        Clock { now: i64::MIN, delete: false }
    } else {
        Clock { now: cx.now, delete: !cx.eng.replica }
    }
}

/// `checkAlreadyExpired`: setting this expiry deletes the field right away.
fn already_expired(cx: &Cx, at: i64) -> bool {
    at <= cx.now && !cx.eng.replica && cx.mode != Mode::Replay
}

fn limits(cx: &Cx) -> Limits {
    Limits::of(cx.cfg())
}

/// The hash at `key` in the current database, without any lookup side effects.
fn hash_ref<'a>(cx: &'a Cx, key: &[u8]) -> Option<&'a Hash> {
    match cx.eng.dbs[cx.client.db].keys.get(key).map(|e| &e.value) {
        Some(Value::Hash(h)) => Some(h),
        _ => None,
    }
}

/// Run `f` on the hash at `key`, keeping the database's memory accounting right. The caller
/// signals the change (`cx.touch`) and emits events.
fn with_hash<R>(cx: &mut Cx, key: &[u8], f: impl FnOnce(&mut Hash) -> R) -> Option<R> {
    let db = cx.client.db;
    let d = &mut cx.eng.dbs[db];
    let Some(Entry { value: Value::Hash(h), .. }) = d.keys.get_mut(key) else { return None };
    let old = h.mem();
    let r = f(h);
    let new = h.mem();
    d.mem = (d.mem + new).saturating_sub(old);
    Some(r)
}

/// `lookupKeyRead` + type check: is there a hash at `key`? (Expiry, LRU, hit/miss stats.)
fn read_hash(cx: &mut Cx, key: &[u8]) -> Result<bool, Error> {
    Ok(cx.get_hash(key)?.is_some())
}

/// `lookupKeyWrite` + type check.
fn write_hash(cx: &mut Cx, key: &[u8]) -> Result<bool, Error> {
    Ok(match cx.get_mut_typed(key, Kind::Hash)? {
        Some(mut e) => {
            e.unchanged();
            true
        }
        None => false,
    })
}

/// Add an empty hash at `key` (absent), emitting `new`. The caller fills it in.
fn create_hash(cx: &mut Cx, key: &[u8]) {
    let lru = cx.eng.new_lru(cx.now);
    let db = cx.client.db;
    cx.eng.dbs[db].insert_new(key, Entry::new(Value::Hash(Box::default()), lru));
    cx.notify(notify::NEW, "new", key);
}

/// `hashTypeLookupWriteOrCreate`.
fn write_or_create(cx: &mut Cx, key: &[u8]) -> Result<(), Error> {
    if !write_hash(cx, key)? {
        create_hash(cx, key);
    }
    Ok(())
}

/// Remove the hash at `key` from the keyspace (signals it; the caller emits `del`).
fn drop_key(cx: &mut Cx, key: &[u8]) {
    let db = cx.client.db;
    if cx.eng.dbs[db].remove(key).is_some() {
        cx.touch(key);
    }
}

/// Delete the hash if it has no fields left, emitting `del`.
fn drop_if_empty(cx: &mut Cx, key: &[u8]) {
    if hash_ref(cx, key).is_some_and(|h| h.is_empty()) {
        drop_key(cx, key);
        cx.notify(notify::GENERIC, "del", key);
    }
}

fn sync_subexpiry(cx: &mut Cx, key: &[u8]) {
    let db = cx.client.db;
    cx.eng.dbs[db].sync_subexpiry(key);
}

/// Unregister the hash when its last volatile field went away (HDEL, HGETDEL).
fn unregister_if_no_ttls(cx: &mut Cx, key: &[u8], had_ttls: bool) {
    if had_ttls && hash_ref(cx, key).is_some_and(|h| !h.is_empty() && !h.has_expiring()) {
        with_hash(cx, key, |h| h.set_subexpiry(0));
        sync_subexpiry(cx, key);
    }
}

/// Propagate the deletion of an expired field (`propagateHashFieldDeletion`): an effect of its
/// own, emitted before the command, that doesn't count as a change made by the command.
fn field_expired_effect(eng: &mut Engine, db: usize, key: &[u8], field: &[u8]) {
    eng.effects.push(Effect { db: db as u16, args: vec![b("HDEL"), Bytes::copy_from_slice(key), Bytes::copy_from_slice(field)] });
    eng.stats.expired_subkeys += 1;
    eng.sig.touch_quiet(db as u16, key);
}

enum Got {
    Value(Bytes),
    Missing,
    /// The field had expired (and was deleted where allowed).
    Expired,
    /// The field had expired and was the last one: the hash was deleted.
    ExpiredHash,
}

/// `hashTypeGetValue`: the value of `field`, deleting it when it has expired (propagated as
/// HDEL). With `notify`, emits `hexpired` (or `del` when the hash empties) like
/// `HFE_LAZY_EXPIRE`; with `keep_hash` an emptied hash is left for the caller to delete
/// (`HFE_LAZY_AVOID_HASH_DEL`).
fn get_field(cx: &mut Cx, key: &[u8], field: &[u8], notify: bool, keep_hash: bool) -> Got {
    let ck = clock(cx);
    let Some((v, at)) = hash_ref(cx, key).and_then(|h| h.get_entry(field)) else { return Got::Missing };
    if !expired(at, ck.now) {
        return Got::Value(Bytes::copy_from_slice(v));
    }
    if !ck.delete {
        return Got::Expired;
    }
    let empty = with_hash(cx, key, |h| {
        h.remove(field);
        h.is_empty()
    })
    .unwrap_or(true);
    let db = cx.client.db;
    field_expired_effect(cx.eng, db, key, field);
    if !empty {
        if notify {
            cx.notify_fields(notify::HASH, "hexpired", key, [field]);
        }
        return Got::Expired;
    }
    if keep_hash {
        return Got::Expired;
    }
    if notify {
        cx.notify(notify::GENERIC, "del", key);
    }
    cx.eng.dbs[db].remove(key);
    Got::ExpiredHash
}

// ---- active field expiry (called by the engine) ----

/// Delete the due fields of the hash at `key` (`hashTypeExpire`), re-registering it with its
/// next field expiry. Returns true if that deleted the whole hash.
pub(crate) fn expire_fields(eng: &mut Engine, db: usize, key: &[u8], now: i64) -> bool {
    let d = &mut eng.dbs[db];
    let Some(Entry { value: Value::Hash(h), .. }) = d.keys.get_mut(key) else {
        // Not a hash (any more): drop a stale index entry.
        d.sync_subexpiry(key);
        return false;
    };
    let old = h.mem();
    let gone = h.expire_due(now);
    h.set_subexpiry(h.min_expire());
    let empty = h.is_empty();
    if !gone.is_empty() && !empty {
        h.shrink();
    }
    let new = h.mem();
    d.mem = (d.mem + new).saturating_sub(old);
    if gone.is_empty() {
        d.sync_subexpiry(key);
        return false;
    }
    for f in &gone {
        field_expired_effect(eng, db, key, f);
    }
    eng.sig.notify_fields(notify::HASH, "hexpired", db as u16, key, &gone);
    if empty {
        eng.sig.notify(notify::GENERIC, "del", db as u16, key);
        eng.dbs[db].remove(key);
    } else {
        eng.dbs[db].sync_subexpiry(key);
    }
    empty
}

/// One active field-expiry step over database `db` (Redis' `activeSubexpiresCycle`): expires
/// the registered hashes whose next field expiry has passed. Returns fields expired.
pub(crate) fn active_expire_fields(eng: &mut Engine, db: usize, now: i64, start: Instant, budget_us: u64) -> usize {
    const SAMPLE: usize = 64;
    if eng.dbs[db].subexpires.is_empty() {
        return 0;
    }
    let mut due: Vec<Str> = Vec::new();
    let mut sampled = 0;
    let mut cursor = eng.dbs[db].subexpire_cursor;
    loop {
        cursor = eng.dbs[db].subexpires.scan(cursor, |k, at| {
            sampled += 1;
            if *at <= now {
                due.push(k.clone());
            }
        });
        if cursor == 0 || sampled >= SAMPLE {
            break;
        }
    }
    eng.dbs[db].subexpire_cursor = cursor;
    let mut total = 0;
    for k in due {
        let before = eng.dbs[db].keys.get(&k).map(|e| match &e.value {
            Value::Hash(h) => h.len(),
            _ => 0,
        });
        match before {
            Some(n) if !eng.dbs[db].keys.get(&k).is_some_and(|e| e.expired(now)) => {
                let deleted = expire_fields(eng, db, &k, now);
                let after = if deleted { 0 } else { hash_len(eng, db, &k) };
                total += n - after;
            }
            Some(_) => {}
            // Stale entry: the key is gone.
            None => eng.dbs[db].sync_subexpiry(&k),
        }
        if start.elapsed().as_micros() as u64 > budget_us {
            break;
        }
    }
    total
}

fn hash_len(eng: &Engine, db: usize, key: &[u8]) -> usize {
    match eng.dbs[db].keys.get(key).map(|e| &e.value) {
        Some(Value::Hash(h)) => h.len(),
        _ => 0,
    }
}

// ---- basic commands ----

/// HSET key field value [field value …] / HMSET (same, replies OK)
pub fn hset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !args.len().is_multiple_of(2) {
        return Err(arity_error(cx.cmd.name));
    }
    let key = &args[1];
    write_or_create(cx, key)?;
    let lim = limits(cx);
    let pairs = (args.len() - 2) / 2;
    let created = with_hash(cx, key, |h| {
        h.prepare(pairs, args[2..].iter().map(|a| a.len()), lim);
        args[2..].chunks(2).filter(|kv| h.set(&kv[0], &kv[1], false, lim)).count()
    })
    .unwrap_or(0);
    cx.touch(key);
    cx.changes(pairs);
    cx.notify_fields(notify::HASH, "hset", key, args[2..].iter().step_by(2));
    Ok(if cx.cmd.name == "hset" { Reply::Int(created as i64) } else { Reply::Ok })
}

pub fn hsetnx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (key, field, value) = (&args[1], &args[2], &args[3]);
    write_or_create(cx, key)?;
    match get_field(cx, key, field, true, false) {
        Got::Value(_) => return Ok(Reply::Int(0)),
        Got::ExpiredHash => create_hash(cx, key),
        _ => {}
    }
    let lim = limits(cx);
    with_hash(cx, key, |h| {
        h.prepare(1, [field.len(), value.len()], lim);
        h.set(field, value, false, lim);
    });
    cx.touch(key);
    cx.notify_fields(notify::HASH, "hset", key, [field]);
    Ok(Reply::Int(1))
}

pub fn hget(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !read_hash(cx, &args[1])? {
        return Ok(Reply::Nil);
    }
    Ok(match get_field(cx, &args[1], &args[2], true, false) {
        Got::Value(v) => Reply::Bulk(v),
        _ => Reply::Nil,
    })
}

pub fn hmget(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let mut exists = read_hash(cx, key)?;
    let mut expired: Vec<&Bytes> = Vec::new();
    let mut deleted = false;
    let mut out = Vec::with_capacity(args.len() - 2);
    for f in &args[2..] {
        if !exists {
            out.push(Reply::Nil);
            continue;
        }
        out.push(match get_field(cx, key, f, false, false) {
            Got::Value(v) => Reply::Bulk(v),
            Got::Expired => {
                expired.push(f);
                Reply::Nil
            }
            Got::ExpiredHash => {
                deleted = true;
                exists = false;
                Reply::Nil
            }
            Got::Missing => Reply::Nil,
        });
    }
    if !expired.is_empty() {
        cx.notify_fields(notify::HASH, "hexpired", key, expired);
    }
    if deleted {
        cx.notify(notify::GENERIC, "del", key);
    }
    Ok(Reply::Array(out))
}

#[derive(Clone, Copy, PartialEq)]
enum What {
    Keys,
    Values,
    Both,
}

fn getall(cx: &mut Cx, key: &[u8], what: What) -> CmdResult {
    let empty = || if what == What::Both { Reply::Map(Vec::new()) } else { Reply::Array(Vec::new()) };
    if !read_hash(cx, key)? {
        return Ok(empty());
    }
    let now = clock(cx).now;
    let h = hash_ref(cx, key).expect("looked up");
    let n = h.live_len(now);
    if n == 0 {
        return Ok(empty());
    }
    let live = h.iter_live(now);
    Ok(match what {
        What::Both => {
            let mut m = Vec::with_capacity(n);
            m.extend(live.map(|(f, v)| (Reply::bulk(f), Reply::bulk(v))));
            Reply::Map(m)
        }
        What::Keys => Reply::Array(live.map(|(f, _)| Reply::bulk(f)).collect()),
        What::Values => Reply::Array(live.map(|(_, v)| Reply::bulk(v)).collect()),
    })
}

pub fn hgetall(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    getall(cx, &args[1], What::Both)
}

pub fn hkeys(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    getall(cx, &args[1], What::Keys)
}

pub fn hvals(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    getall(cx, &args[1], What::Values)
}

pub fn hdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    if !write_hash(cx, key)? {
        return Ok(Reply::Int(0));
    }
    let had_ttls = hash_ref(cx, key).is_some_and(|h| h.has_expiring());
    let (gone, removed) = with_hash(cx, key, |h| {
        let mut gone: Vec<&Bytes> = Vec::new();
        for f in &args[2..] {
            if h.remove(f) {
                gone.push(f);
                if h.is_empty() {
                    return (gone, true);
                }
            }
        }
        (gone, false)
    })
    .unwrap_or_default();
    let n = gone.len() as i64;
    if n > 0 {
        cx.changes(gone.len());
        if removed {
            drop_key(cx, key);
        } else {
            unregister_if_no_ttls(cx, key, had_ttls);
            cx.touch(key);
        }
        cx.notify_fields(notify::HASH, "hdel", key, gone);
        if removed {
            cx.notify(notify::GENERIC, "del", key);
        }
    }
    Ok(Reply::Int(n))
}

pub fn hlen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.get_hash(&args[1])?.map_or(0, |h| h.len()) as i64))
}

pub fn hstrlen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !read_hash(cx, &args[1])? {
        return Ok(Reply::Int(0));
    }
    Ok(Reply::Int(match get_field(cx, &args[1], &args[2], true, false) {
        Got::Value(v) => v.len() as i64,
        _ => 0,
    }))
}

pub fn hexists(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !read_hash(cx, &args[1])? {
        return Ok(Reply::Int(0));
    }
    Ok(Reply::Int(matches!(get_field(cx, &args[1], &args[2], true, false), Got::Value(_)) as i64))
}

pub fn hincrby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let incr = int(&args[3])?;
    let (key, field) = (&args[1], &args[2]);
    write_or_create(cx, key)?;
    let cur = match get_field(cx, key, field, true, false) {
        Got::Value(v) => parse_i64(&v).ok_or(NOT_INT_VALUE)?,
        Got::Missing | Got::Expired => 0,
        Got::ExpiredHash => {
            create_hash(cx, key);
            0
        }
    };
    let new = cur.checked_add(incr).ok_or(OVERFLOW)?;
    let s = fmt_i64(new);
    let lim = limits(cx);
    with_hash(cx, key, |h| {
        h.prepare(1, [field.len(), s.len()], lim);
        h.set(field, &s, true, lim);
    });
    cx.touch(key);
    cx.notify_fields(notify::HASH, "hincrby", key, [field]);
    Ok(Reply::Int(new))
}

/// `inf`/`infinity` (any case, optional sign): parses as a long double but is rejected.
fn is_inf(s: &[u8]) -> bool {
    let l = s.to_ascii_lowercase();
    let b = l.strip_prefix(b"-").or_else(|| l.strip_prefix(b"+")).unwrap_or(&l);
    b == b"inf" || b == b"infinity"
}

/// Would `string2ld` accept this (finite, or an infinity literal)?
fn long_double_ok(s: &[u8]) -> bool {
    decimal::valid(s)
}

pub fn hincrbyfloat(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (key, field, incr) = (&args[1], &args[2], &args[3]);
    if !long_double_ok(incr) {
        return Err(NOT_FLOAT);
    }
    if is_inf(incr) {
        return Err(Error::new("ERR value is NaN or Infinity"));
    }
    write_or_create(cx, key)?;
    let cur: Bytes = match get_field(cx, key, field, true, false) {
        Got::Value(v) => v,
        Got::Missing | Got::Expired => b("0"),
        Got::ExpiredHash => {
            create_hash(cx, key);
            b("0")
        }
    };
    if !long_double_ok(&cur) {
        return Err(NOT_FLOAT_VALUE);
    }
    let new = decimal::incr_by_float(&cur, incr).map_err(|e| match e {
        IncrError::NotFloat => NOT_FLOAT_VALUE,
        IncrError::NanOrInf => Error::new("ERR increment would produce NaN or Infinity"),
    })?;
    let lim = limits(cx);
    with_hash(cx, key, |h| {
        h.prepare(1, [field.len(), new.len()], lim);
        h.set(field, new.as_bytes(), true, lim);
    });
    cx.touch(key);
    cx.notify_fields(notify::HASH, "hincrbyfloat", key, [field]);
    // Replicated as the final value, keeping the field's TTL.
    let nb = Bytes::from(new);
    cx.propagate_as(vec![b("HSETEX"), key.clone(), b("KEEPTTL"), b("FIELDS"), b("1"), field.clone(), nb.clone()]);
    Ok(Reply::Bulk(nb))
}

// ---- HRANDFIELD ----

/// HRANDFIELD expires every expired field first (`hashTypeExpireIfNeeded`). True if that
/// deleted the hash.
fn expire_all_if_needed(cx: &mut Cx, key: &[u8]) -> bool {
    let min = hash_ref(cx, key).map_or(0, |h| h.min_expire());
    if min == 0 || min >= cx.now || !clock(cx).delete {
        return false;
    }
    let db = cx.client.db;
    let now = cx.now;
    expire_fields(cx.eng, db, key, now)
}

/// HRANDFIELD key [count [WITHVALUES]]
pub fn hrandfield(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    if args.len() >= 3 {
        let l = int(&args[2])?;
        if l == i64::MIN {
            return Err(Error::new(
                "ERR value is out of range, value must between -9223372036854775807 and 9223372036854775807",
            ));
        }
        if args.len() > 4 || (args.len() == 4 && !eq(&args[3], "withvalues")) {
            return Err(SYNTAX);
        }
        let withvalues = args.len() == 4;
        if withvalues && !(-(i64::MAX / 2)..=i64::MAX / 2).contains(&l) {
            return Err(Error::new("ERR value is out of range"));
        }
        return hrandfield_count(cx, key, l, withvalues);
    }
    if !read_hash(cx, key)? || expire_all_if_needed(cx, key) {
        return Ok(Reply::Nil);
    }
    let Engine { dbs, rng, .. } = &mut *cx.eng;
    let Some(Entry { value: Value::Hash(h), .. }) = dbs[cx.client.db].keys.get(key) else { return Ok(Reply::Nil) };
    Ok(h.random(rng).map_or(Reply::Nil, |(f, _)| Reply::bulk(f)))
}

fn hrandfield_count(cx: &mut Cx, key: &[u8], l: i64, withvalues: bool) -> CmdResult {
    if !read_hash(cx, key)? || expire_all_if_needed(cx, key) {
        return Ok(Reply::Array(Vec::new()));
    }
    let (count, uniq) = if l >= 0 { (l as u64, true) } else { (l.unsigned_abs(), false) };
    if count == 0 {
        return Ok(Reply::Array(Vec::new()));
    }
    let resp3 = cx.resp() >= 3;
    let Engine { dbs, rng, .. } = &mut *cx.eng;
    let Some(Entry { value: Value::Hash(h), .. }) = dbs[cx.client.db].keys.get(key) else {
        return Ok(Reply::Array(Vec::new()));
    };
    let size = h.len() as u64;
    let mut out = Vec::new();
    let mut emit = |f: &[u8], v: &[u8]| {
        if withvalues && resp3 {
            out.push(Reply::Array(vec![Reply::bulk(f), Reply::bulk(v)]));
        } else {
            out.push(Reply::bulk(f));
            if withvalues {
                out.push(Reply::bulk(v));
            }
        }
    };
    if !uniq || count == 1 {
        // CASE 1: sampling with repetitions.
        for _ in 0..count {
            let (f, v) = h.random(rng).expect("not empty");
            emit(f, v);
        }
    } else if count >= size {
        // CASE 2: the whole hash.
        for (f, v, _) in h.iter() {
            emit(f, v);
        }
    } else if h.is_listpack() {
        // CASE 2.5: a uniform subset in listpack order (selection sampling).
        let mut need = count;
        for i in 0..size {
            if (rng.below((size - i) as usize) as u64) < need {
                let (f, v) = h.pack_pair(i as usize).expect("in range");
                emit(f, v);
                need -= 1;
                if need == 0 {
                    break;
                }
            }
        }
    } else if count * 3 > size {
        // CASE 3: copy everything, then discard random elements.
        let mut all: Vec<(&[u8], &[u8])> = h.iter().map(|(f, v, _)| (f, v)).collect();
        while all.len() as u64 > count {
            let i = rng.below(all.len());
            all.swap_remove(i);
        }
        for (f, v) in all {
            emit(f, v);
        }
    } else {
        // CASE 4: random picks until enough distinct fields.
        let mut seen = std::collections::HashSet::new();
        while (seen.len() as u64) < count {
            let (f, v) = h.random(rng).expect("not empty");
            if seen.insert(f) {
                emit(f, v);
            }
        }
    }
    Ok(Reply::Array(out))
}

// ---- HSCAN ----

/// HSCAN key cursor [MATCH pattern] [COUNT count] [NOVALUES]
pub fn hscan(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let cursor = parse_cursor(&args[2])?;
    let key = &args[1];
    if !read_hash(cx, key)? {
        return Ok(scan_reply(0, Vec::new()));
    }
    let o = ScanOpts::parse(&args[3..], false)?;
    let now = clock(cx).now;
    let h = hash_ref(cx, key).expect("looked up");
    let mut items = Vec::new();
    let mut push = |f: &[u8], v: &[u8], at: i64| {
        if expired(at, now) || !o.matches(f) {
            return;
        }
        items.push(Reply::bulk(f));
        if !o.novalues {
            items.push(Reply::bulk(v));
        }
    };
    if h.is_listpack() {
        for (f, v, at) in h.iter() {
            push(f, v, at);
        }
        return Ok(scan_reply(0, items));
    }
    let mut c = cursor;
    let mut sampled = 0usize;
    let mut left = o.count.saturating_mul(10);
    loop {
        c = h
            .scan_table(c, |f, v, at| {
                sampled += 1;
                push(f, v, at);
            })
            .unwrap_or(0);
        if c == 0 || left == 0 {
            break;
        }
        left -= 1;
        if sampled >= o.count {
            break;
        }
    }
    Ok(scan_reply(c, items))
}

// ---- field expiration ----

/// `FIELDS numfields field…` at a fixed position (HTTL, HPERSIST, HGETDEL): the fields.
fn fixed_fields(args: &[Bytes]) -> Result<&[Bytes], Error> {
    if !eq(&args[2], "fields") {
        return Err(FIELDS_POSITION);
    }
    let n = parse_i64(&args[3]).filter(|&n| n >= 1).ok_or(NUMFIELDS_POSITIVE)?;
    if n as usize != args.len() - 4 {
        return Err(NUMFIELDS_MATCH);
    }
    Ok(&args[4..])
}

/// `parseExpireTime`: absolute ms from a (relative when `base` is now) seconds/ms argument.
fn expire_time(cx: &Cx, a: &[u8], secs: bool, base: i64) -> Result<i64, Error> {
    let mut v = int(a)?;
    if v < 0 {
        return Err(Error::new("ERR invalid expire time, must be >= 0"));
    }
    let invalid = || Error::msg(format!("ERR invalid expire time in '{}' command", cx.cmd.name));
    if secs {
        if v > MAX_EXPIRE_MS / 1000 {
            return Err(invalid());
        }
        v *= 1000;
    }
    if v > MAX_EXPIRE_MS - base {
        return Err(invalid());
    }
    Ok(v + base)
}

#[derive(Clone, Copy, PartialEq)]
enum Cond {
    None,
    Nx,
    Xx,
    Gt,
    Lt,
}

/// State of a batch of field-expiry updates (`HashTypeSetEx`).
struct SetEx {
    cond: Cond,
    /// The hash's registered next expiry when the batch started (`NO_EXPIRY` if none).
    min_before: i64,
    /// Smallest old/new expiry among the updated fields.
    min_fields: i64,
}

/// `hashTypeSetExInit`: makes a listpack hash `listpackex`.
fn setex_init(cx: &mut Cx, key: &[u8], cond: Cond) -> SetEx {
    let reg = with_hash(cx, key, |h| {
        h.enable_ttl();
        h.subexpiry()
    })
    .unwrap_or(0);
    SetEx { cond, min_before: if reg == 0 { NO_EXPIRY } else { reg }, min_fields: NO_EXPIRY }
}

/// `hashTypeSetEx`: set (`at > 0`) or remove (`at == 0`) the expiry of one field.
fn setex_field(cx: &mut Cx, key: &[u8], field: &[u8], at: i64, ex: &mut SetEx) -> i64 {
    let Some(h) = hash_ref(cx, key) else { return NO_FIELD };
    let table = !h.is_listpack();
    let Some(prev) = h.expire_of(field) else { return NO_FIELD };
    if at == 0 {
        if prev == 0 {
            return NO_CONDITION;
        }
        with_hash(cx, key, |h| h.set_expire(field, 0));
        return UPDATED;
    }
    if prev == 0 {
        // A field without TTL counts as an infinite one: LT holds, GT doesn't.
        if matches!(ex.cond, Cond::Xx | Cond::Gt) {
            return NO_CONDITION;
        }
    } else {
        if (ex.cond == Cond::Gt && prev >= at) || (ex.cond == Cond::Lt && prev <= at) || ex.cond == Cond::Nx {
            return NO_CONDITION;
        }
        if table && prev == at {
            return UPDATED;
        }
        ex.min_fields = ex.min_fields.min(prev);
    }
    if already_expired(cx, at) {
        with_hash(cx, key, |h| h.remove(field));
        let db = cx.client.db;
        field_expired_effect(cx.eng, db, key, field);
        return DELETED;
    }
    ex.min_fields = ex.min_fields.min(at);
    with_hash(cx, key, |h| h.set_expire(field, at));
    UPDATED
}

/// `hashTypeSetExDone`: re-register the hash if its next expiry moved enough.
fn setex_done(cx: &mut Cx, key: &[u8], ex: &SetEx) {
    let Some(h) = hash_ref(cx, key) else { return };
    if h.is_empty() || ex.min_before < ex.min_fields {
        return;
    }
    let next = match h.min_expire() {
        0 => NO_EXPIRY,
        at => at,
    };
    if (ex.min_before - next).abs() < REREGISTER_DIFF {
        return;
    }
    with_hash(cx, key, |h| h.set_subexpiry(if next == NO_EXPIRY { 0 } else { next }));
    sync_subexpiry(cx, key);
}

/// HEXPIRE / HPEXPIRE / HEXPIREAT / HPEXPIREAT key time [NX | XX | GT | LT] FIELDS numfields field…
fn hexpire_generic(cx: &mut Cx, args: &[Bytes], secs: bool, relative: bool) -> CmdResult {
    let key = &args[1];
    let found = write_hash(cx, key)?;
    let at = expire_time(cx, &args[2], secs, if relative { cx.now } else { 0 })?;
    let mut fields_at = None;
    let mut flags = Vec::new();
    let mut i = 3;
    while i < args.len() {
        let a = &args[i];
        if eq(a, "fields") {
            if fields_at.is_some() {
                return Err(Error::new("ERR FIELDS keyword specified multiple times"));
            }
            if i + 2 >= args.len() {
                return Err(Error::new("ERR FIELDS requires at least numfields and one field argument"));
            }
            let n = parse_i64(&args[i + 1])
                .filter(|n| (1..=i32::MAX as i64).contains(n))
                .ok_or(Error::new("ERR Parameter `numFields` should be greater than 0"))? as usize;
            if n > args.len() - (i + 2) {
                return Err(Error::new("ERR wrong number of arguments"));
            }
            fields_at = Some((i, n));
            i += 2 + n;
            continue;
        }
        let f = if eq(a, "nx") {
            Cond::Nx
        } else if eq(a, "xx") {
            Cond::Xx
        } else if eq(a, "gt") {
            Cond::Gt
        } else if eq(a, "lt") {
            Cond::Lt
        } else {
            return Err(Error::msg(format!("ERR unknown argument: {}", String::from_utf8_lossy(a))));
        };
        if !flags.contains(&f) {
            flags.push(f);
        }
        i += 1;
    }
    let Some((fields_pos, n)) = fields_at else { return Err(Error::new("ERR missing FIELDS argument")) };
    if flags.len() > 1 {
        return Err(Error::new("ERR Multiple condition flags specified"));
    }
    let cond = flags.first().copied().unwrap_or(Cond::None);
    let fields = &args[fields_pos + 2..fields_pos + 2 + n];
    if !found {
        return Ok(Reply::Array(vec![Reply::Int(NO_FIELD); n]));
    }
    let mut ex = setex_init(cx, key, cond);
    let mut out = Vec::with_capacity(n);
    let mut kept = Vec::with_capacity(n);
    let mut deleted = Vec::new();
    for f in fields {
        let r = setex_field(cx, key, f, at, &mut ex);
        match r {
            UPDATED => kept.push(f.clone()),
            DELETED => deleted.push(f),
            _ => {}
        }
        out.push(Reply::Int(r));
    }
    setex_done(cx, key, &ex);
    if !kept.is_empty() || !deleted.is_empty() {
        cx.touch(key);
        cx.changes(kept.len() + deleted.len());
        if !deleted.is_empty() {
            cx.notify_fields(notify::HASH, "hdel", key, deleted);
        }
        if !kept.is_empty() {
            cx.notify_fields(notify::HASH, "hexpire", key, &kept);
        }
    }
    drop_if_empty(cx, key);
    if kept.is_empty() {
        cx.no_propagate();
    } else {
        // Propagate as HPEXPIREAT with only the fields that were updated.
        let mut prop: Vec<Bytes> = Vec::with_capacity(args.len());
        prop.push(if cx.cmd.name == "hpexpireat" { args[0].clone() } else { b("HPEXPIREAT") });
        prop.push(key.clone());
        prop.push(if cx.cmd.name == "hpexpireat" { args[2].clone() } else { int_bytes(at) });
        prop.extend_from_slice(&args[3..fields_pos + 1]);
        prop.push(int_bytes(kept.len() as i64));
        prop.extend(kept);
        prop.extend_from_slice(&args[fields_pos + 2 + n..]);
        cx.propagate_as(prop);
    }
    Ok(Reply::Array(out))
}

pub fn hexpire(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    hexpire_generic(cx, args, true, true)
}
pub fn hpexpire(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    hexpire_generic(cx, args, false, true)
}
pub fn hexpireat(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    hexpire_generic(cx, args, true, false)
}
pub fn hpexpireat(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    hexpire_generic(cx, args, false, false)
}

/// HTTL / HPTTL / HEXPIRETIME / HPEXPIRETIME key FIELDS numfields field…
fn httl_generic(cx: &mut Cx, args: &[Bytes], ms: bool, relative: bool) -> CmdResult {
    let key = &args[1];
    let found = read_hash(cx, key)?;
    let fields = fixed_fields(args)?;
    if !found {
        return Ok(Reply::Array(vec![Reply::Int(NO_FIELD); fields.len()]));
    }
    let now = cx.now;
    let base = if relative { now } else { 0 };
    let h = hash_ref(cx, key).expect("looked up");
    // listpackex hides a field expiring right now; the hashtable shows it for that millisecond.
    let gone = |at: i64| if h.is_listpack() { at <= now } else { at < now };
    let out = fields
        .iter()
        .map(|f| {
            Reply::Int(match h.expire_of(f) {
                None => NO_FIELD,
                Some(0) => NO_TTL,
                Some(at) if gone(at) => NO_FIELD,
                Some(at) if ms => at - base,
                Some(at) => (at + 999 - base) / 1000,
            })
        })
        .collect();
    Ok(Reply::Array(out))
}

pub fn httl(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    httl_generic(cx, args, false, true)
}
pub fn hpttl(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    httl_generic(cx, args, true, true)
}
pub fn hexpiretime(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    httl_generic(cx, args, false, false)
}
pub fn hpexpiretime(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    httl_generic(cx, args, true, false)
}

/// HPERSIST key FIELDS numfields field…
pub fn hpersist(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let found = write_hash(cx, key)?;
    let fields = fixed_fields(args)?;
    if !found {
        return Ok(Reply::Array(vec![Reply::Int(NO_FIELD); fields.len()]));
    }
    let now = clock(cx).now;
    let mut out = Vec::with_capacity(fields.len());
    let mut persisted = Vec::new();
    for f in fields {
        let r = match hash_ref(cx, key).and_then(|h| h.expire_of(f)) {
            None => NO_FIELD,
            Some(0) => NO_TTL,
            Some(at) if at < now => NO_FIELD,
            Some(_) => {
                with_hash(cx, key, |h| h.set_expire(f, 0));
                persisted.push(f);
                UPDATED
            }
        };
        out.push(Reply::Int(r));
    }
    if !persisted.is_empty() {
        cx.notify_fields(notify::HASH, "hpersist", key, persisted);
        cx.touch(key);
    }
    Ok(Reply::Array(out))
}

/// HGETDEL key FIELDS numfields field…
pub fn hgetdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let found = write_hash(cx, key)?;
    let fields = fixed_fields(args)?;
    if !found {
        return Ok(Reply::Array(vec![Reply::Nil; fields.len()]));
    }
    let had_ttls = hash_ref(cx, key).is_some_and(|h| h.has_expiring());
    let mut out = Vec::with_capacity(fields.len());
    let (mut expired, mut deleted) = (Vec::new(), Vec::new());
    for f in fields {
        out.push(match get_field(cx, key, f, false, true) {
            Got::Value(v) => {
                with_hash(cx, key, |h| h.remove(f));
                deleted.push(f);
                Reply::Bulk(v)
            }
            Got::Expired | Got::ExpiredHash => {
                expired.push(f);
                Reply::Nil
            }
            Got::Missing => Reply::Nil,
        });
    }
    if expired.is_empty() && deleted.is_empty() {
        return Ok(Reply::Array(out));
    }
    let empty = hash_ref(cx, key).is_none_or(|h| h.is_empty());
    if !empty {
        unregister_if_no_ttls(cx, key, had_ttls);
    }
    cx.touch(key);
    cx.changes(deleted.len());
    if !expired.is_empty() {
        cx.notify_fields(notify::HASH, "hexpired", key, expired);
    }
    if !deleted.is_empty() {
        cx.notify_fields(notify::HASH, "hdel", key, deleted);
        let mut prop = vec![b("HDEL"), key.clone()];
        prop.extend_from_slice(fields);
        cx.propagate_as(prop);
    } else {
        cx.no_propagate();
    }
    if empty {
        drop_key(cx, key);
        cx.notify(notify::GENERIC, "del", key);
    }
    Ok(Reply::Array(out))
}

#[derive(Clone, Copy, PartialEq)]
enum Ttl {
    None,
    /// EX / PX / EXAT / PXAT: absolute ms, with the position of the option name.
    At(i64, usize, bool),
    Persist,
    Keep,
}

struct ExArgs {
    ttl: Ttl,
    fxx: bool,
    fnx: bool,
    /// Index of the first field and the field count.
    first: usize,
    n: usize,
}

/// `parseHashFieldExpireArgs` for HGETEX (`set == false`) and HSETEX (`set == true`).
fn parse_ex_args(cx: &Cx, args: &[Bytes], set: bool) -> Result<ExArgs, Error> {
    let mut o = ExArgs { ttl: Ttl::None, fxx: false, fnx: false, first: 0, n: 0 };
    let mut fields = false;
    let only_one = if set {
        Error::new("ERR Only one of EX, PX, EXAT, PXAT or KEEPTTL arguments can be specified")
    } else {
        Error::new("ERR Only one of EX, PX, EXAT, PXAT or PERSIST arguments can be specified")
    };
    let mut i = 2;
    while i < args.len() {
        let a = &args[i];
        if eq(a, "fields") {
            if fields {
                return Err(Error::new("ERR FIELDS keyword specified multiple times"));
            }
            if i + 1 >= args.len() {
                return Err(arity_error(cx.cmd.name));
            }
            let n = parse_i64(&args[i + 1])
                .filter(|n| (1..=i32::MAX as i64).contains(n))
                .ok_or(Error::new("ERR invalid number of fields"))? as usize;
            let per = if set { 2 } else { 1 };
            if i + 2 + n * per > args.len() {
                return Err(Error::new("ERR wrong number of arguments"));
            }
            fields = true;
            o.first = i + 2;
            o.n = n;
            i = i + 2 + n * per;
            continue;
        }
        let unit = if eq(a, "ex") {
            Some((true, true))
        } else if eq(a, "px") {
            Some((false, true))
        } else if eq(a, "exat") {
            Some((true, false))
        } else if eq(a, "pxat") {
            Some((false, false))
        } else {
            None
        };
        if let Some((secs, relative)) = unit {
            if o.ttl != Ttl::None {
                return Err(only_one);
            }
            if i >= args.len() - 1 {
                return Err(Error::new("ERR missing expire time"));
            }
            let at = expire_time(cx, &args[i + 1], secs, if relative { cx.now } else { 0 })?;
            o.ttl = Ttl::At(at, i, eq(a, "pxat"));
            i += 2;
            continue;
        }
        if !set && eq(a, "persist") {
            if o.ttl != Ttl::None {
                return Err(only_one);
            }
            o.ttl = Ttl::Persist;
        } else if set && eq(a, "keepttl") {
            if o.ttl != Ttl::None {
                return Err(only_one);
            }
            o.ttl = Ttl::Keep;
        } else if set && (eq(a, "fxx") || eq(a, "fnx")) {
            if o.fxx || o.fnx {
                return Err(Error::new("ERR Only one of FXX or FNX arguments can be specified"));
            }
            if eq(a, "fxx") {
                o.fxx = true;
            } else {
                o.fnx = true;
            }
        } else {
            return Err(Error::msg(format!("ERR unknown argument: {}", String::from_utf8_lossy(a))));
        }
        i += 1;
    }
    if !fields {
        return Err(Error::new("ERR missing FIELDS argument"));
    }
    Ok(o)
}

/// HGETEX key [EX s | PX ms | EXAT ts | PXAT ms | PERSIST] FIELDS numfields field…
pub fn hgetex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let found = write_hash(cx, key)?;
    let o = parse_ex_args(cx, args, false)?;
    let fields = &args[o.first..o.first + o.n];
    if !found {
        return Ok(Reply::Array(vec![Reply::Nil; o.n]));
    }
    let change = o.ttl != Ttl::None;
    let at = match o.ttl {
        Ttl::At(at, _, _) => at,
        _ => 0,
    };
    let mut ex = if change { Some(setex_init(cx, key, Cond::None)) } else { None };
    let mut out = Vec::with_capacity(o.n);
    let (mut expired, mut deleted, mut updated) = (Vec::new(), Vec::new(), Vec::new());
    for f in fields {
        match get_field(cx, key, f, false, true) {
            Got::Value(v) => {
                out.push(Reply::Bulk(v));
                if let Some(ex) = ex.as_mut() {
                    match setex_field(cx, key, f, at, ex) {
                        DELETED => deleted.push(f),
                        UPDATED => updated.push(f),
                        _ => {}
                    }
                }
            }
            Got::Expired | Got::ExpiredHash => {
                expired.push(f);
                out.push(Reply::Nil);
            }
            Got::Missing => out.push(Reply::Nil),
        }
    }
    if let Some(ex) = &ex {
        setex_done(cx, key, ex);
    }
    if expired.is_empty() && deleted.is_empty() && updated.is_empty() {
        return Ok(Reply::Array(out));
    }
    cx.touch(key);
    cx.changes(deleted.len() + updated.len());
    if !expired.is_empty() {
        cx.notify_fields(notify::HASH, "hexpired", key, expired);
    }
    if !updated.is_empty() {
        let mut prop = if o.ttl == Ttl::Persist {
            cx.notify_fields(notify::HASH, "hpersist", key, updated);
            vec![b("HPERSIST"), key.clone()]
        } else {
            cx.notify_fields(notify::HASH, "hexpire", key, updated);
            vec![b("HPEXPIREAT"), key.clone(), int_bytes(at)]
        };
        prop.push(b("FIELDS"));
        prop.push(int_bytes(o.n as i64));
        prop.extend_from_slice(fields);
        cx.propagate_as(prop);
    } else {
        if !deleted.is_empty() {
            cx.notify_fields(notify::HASH, "hdel", key, deleted);
        }
        // Lazily expired or deleted fields were propagated as HDELs.
        cx.no_propagate();
    }
    drop_if_empty(cx, key);
    Ok(Reply::Array(out))
}

/// HSETEX key [FNX | FXX] [EX s | PX ms | EXAT ts | PXAT ms | KEEPTTL] FIELDS numfields field value…
pub fn hsetex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let o = parse_ex_args(cx, args, true)?;
    if !write_hash(cx, key)? {
        if o.fxx {
            return Ok(Reply::Int(0));
        }
        create_hash(cx, key);
    }
    let pair = |i: usize| (&args[o.first + 2 * i], &args[o.first + 2 * i + 1]);
    let mut expired = Vec::new();
    let mut ok = true;
    if o.fxx || o.fnx {
        let mut found = 0;
        for i in 0..o.n {
            let exists = match get_field(cx, key, pair(i).0, false, true) {
                Got::Value(_) => true,
                Got::Expired | Got::ExpiredHash => {
                    expired.push(pair(i).0);
                    false
                }
                Got::Missing => false,
            };
            found += exists as usize;
            if (o.fxx && !exists) || (o.fnx && exists) {
                break;
            }
        }
        ok = !((o.fnx && found != 0) || (o.fxx && found != o.n));
    }
    let (mut deleted, mut updated) = (Vec::new(), Vec::new());
    if ok {
        let lim = limits(cx);
        let tail = &args[o.first..];
        with_hash(cx, key, |h| h.prepare(tail.len() / 2, tail.iter().map(|a| a.len()), lim));
        let at = match o.ttl {
            Ttl::At(at, _, _) => Some(at),
            _ => None,
        };
        let mut ex = at.map(|_| setex_init(cx, key, Cond::None));
        let keep = o.ttl != Ttl::None;
        for i in 0..o.n {
            let (f, v) = pair(i);
            with_hash(cx, key, |h| h.set(f, v, keep, lim));
            if let (Some(at), Some(ex)) = (at, ex.as_mut()) {
                match setex_field(cx, key, f, at, ex) {
                    UPDATED => updated.push(f),
                    DELETED => deleted.push(f),
                    _ => {}
                }
            }
        }
        if let Some(ex) = &ex {
            setex_done(cx, key, ex);
        }
        if !deleted.is_empty() {
            // The fields' HDELs were propagated already.
            cx.no_propagate();
        } else if let Ttl::At(at, pos, false) = o.ttl {
            let mut prop = args.to_vec();
            prop[pos] = b("PXAT");
            prop[pos + 1] = int_bytes(at);
            cx.propagate_as(prop);
        }
    } else {
        cx.no_propagate();
    }
    if ok || !expired.is_empty() {
        cx.touch(key);
        cx.changes(if ok { o.n } else { 0 });
        if !expired.is_empty() {
            cx.notify_fields(notify::HASH, "hexpired", key, expired);
        }
        if ok {
            cx.notify_fields(notify::HASH, "hset", key, (0..o.n).map(|i| pair(i).0));
            if !deleted.is_empty() {
                cx.notify_fields(notify::HASH, "hdel", key, deleted);
            } else if !updated.is_empty() {
                cx.notify_fields(notify::HASH, "hexpire", key, updated);
            }
        }
        drop_if_empty(cx, key);
    }
    Ok(Reply::Int(ok as i64))
}

#[cfg(test)]
mod tests {
    use crate::{Client, Engine, Reply};
    use bytes::Bytes;

    fn run(e: &mut Engine, c: &mut Client, line: &str) -> Reply {
        let args: Vec<Bytes> = line.split_whitespace().map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
        e.call(c, &args).unwrap_reply()
    }

    fn bulk(s: &str) -> Reply {
        Reply::bulk(s)
    }

    #[test]
    fn basics_and_encodings() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        assert_eq!(run(&mut e, &mut c, "HSET h a 1 b 2"), Reply::Int(2));
        assert_eq!(run(&mut e, &mut c, "HSET h a 3"), Reply::Int(0));
        assert_eq!(run(&mut e, &mut c, "HGET h a"), bulk("3"));
        assert_eq!(run(&mut e, &mut c, "HLEN h"), Reply::Int(2));
        assert_eq!(run(&mut e, &mut c, "OBJECT ENCODING h"), bulk("listpack"));
        assert_eq!(run(&mut e, &mut c, "HINCRBY h a 5"), Reply::Int(8));
        assert_eq!(run(&mut e, &mut c, "HINCRBYFLOAT h a 0.5"), bulk("8.5"));
        assert_eq!(run(&mut e, &mut c, "HDEL h a b c"), Reply::Int(2));
        assert_eq!(run(&mut e, &mut c, "EXISTS h"), Reply::Int(0));
        assert_eq!(run(&mut e, &mut c, "HSET h x 1234567890123456789012345678901234567890123456789012345678901234567890"), Reply::Int(1));
        assert_eq!(run(&mut e, &mut c, "OBJECT ENCODING h"), bulk("hashtable"));
        assert!(matches!(run(&mut e, &mut c, "HSET h a"), Reply::Error(_)));
        run(&mut e, &mut c, "SET s v");
        assert!(matches!(run(&mut e, &mut c, "HGET s a"), Reply::Error(m) if m.starts_with("WRONGTYPE")));
    }

    #[test]
    fn field_ttls_expire_and_register() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        run(&mut e, &mut c, "HSET h a 1 b 2");
        assert_eq!(run(&mut e, &mut c, "HPEXPIRE h 100000 FIELDS 1 a"), Reply::Array(vec![Reply::Int(1)]));
        assert_eq!(run(&mut e, &mut c, "OBJECT ENCODING h"), bulk("listpackex"));
        assert_eq!(e.dbs[0].subexpires.len(), 1);
        // Expire it "in the past" by rewriting the field's expiry directly.
        assert_eq!(run(&mut e, &mut c, "HPEXPIREAT h 1 FIELDS 1 b"), Reply::Array(vec![Reply::Int(2)]));
        assert_eq!(run(&mut e, &mut c, "HKEYS h"), Reply::Array(vec![bulk("a")]));
        let effects = e.take_effects();
        assert!(effects.iter().any(|x| x.args[0].as_ref() == b"HDEL"));
        assert!(effects.iter().any(|x| x.args[0].as_ref() == b"HPEXPIREAT"));
        // Active expiry deletes due fields of registered hashes.
        if let Some(crate::Entry { value: crate::Value::Hash(h), .. }) = e.dbs[0].keys.get_mut(&b"h"[..]) {
            h.set_expire(b"a", 1);
            h.set_subexpiry(1);
        }
        e.dbs[0].sync_subexpiry(b"h");
        e.active_expire(1_000_000);
        assert_eq!(run(&mut e, &mut c, "EXISTS h"), Reply::Int(0));
        assert_eq!(e.dbs[0].subexpires.len(), 0);
        let effects = e.take_effects();
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].args, vec![Bytes::from("HDEL"), Bytes::from("h"), Bytes::from("a")]);
    }

    #[test]
    fn hincrbyfloat_propagates_hsetex() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        run(&mut e, &mut c, "HINCRBYFLOAT h f 1.5");
        let eff = e.take_effects();
        let args: Vec<&[u8]> = eff[0].args.iter().map(|a| a.as_ref()).collect();
        assert_eq!(args, vec![&b"HSETEX"[..], b"h", b"KEEPTTL", b"FIELDS", b"1", b"f", b"1.5"]);
        run(&mut e, &mut c, "HEXPIRE h 100 FIELDS 1 f");
        let eff = e.take_effects();
        assert_eq!(eff[0].args[0].as_ref(), b"HPEXPIREAT");
    }

    fn effects(e: &mut Engine) -> Vec<String> {
        e.take_effects()
            .iter()
            .map(|x| x.args.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect::<Vec<_>>().join(" "))
            .collect()
    }

    #[test]
    fn propagation_rewrites_and_replay() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        run(&mut e, &mut c, "HSET h a 1 b 2 c 3");
        effects(&mut e);
        // Only updated fields, relative time made absolute; a deleted field goes as HDEL first.
        run(&mut e, &mut c, "HPEXPIREAT h 9999999999999 FIELDS 1 c");
        run(&mut e, &mut c, "HEXPIRE h 100 NX FIELDS 3 a c nofield");
        let eff = effects(&mut e);
        assert_eq!(eff[0], "HPEXPIREAT h 9999999999999 FIELDS 1 c");
        assert!(eff[1].starts_with("HPEXPIREAT h ") && eff[1].ends_with(" NX FIELDS 1 a"), "{eff:?}");
        run(&mut e, &mut c, "HEXPIRE h 100 GT FIELDS 1 b");
        assert!(effects(&mut e).is_empty());
        run(&mut e, &mut c, "HSETEX h EX 100 FIELDS 1 d 4");
        let eff = effects(&mut e);
        assert!(eff[0].starts_with("HSETEX h PXAT ") && eff[0].ends_with(" FIELDS 1 d 4"), "{eff:?}");
        run(&mut e, &mut c, "HGETDEL h FIELDS 2 d nofield");
        assert_eq!(effects(&mut e), vec!["HDEL h d nofield"]);
        run(&mut e, &mut c, "HPEXPIREAT h 1 FIELDS 1 b");
        assert_eq!(effects(&mut e), vec!["HDEL h b"]);
        run(&mut e, &mut c, "HGETEX h PERSIST FIELDS 1 a");
        assert_eq!(effects(&mut e), vec!["HPERSIST h FIELDS 1 a"]);
        // Reads don't propagate.
        run(&mut e, &mut c, "HGETALL h");
        run(&mut e, &mut c, "HRANDFIELD h 2");
        assert!(effects(&mut e).is_empty());

        // A replica applying a past expiry keeps the field until the primary's HDEL arrives.
        let mut r = Engine::standalone();
        r.replica = true;
        let mk = |s: &str| crate::engine::Effect { db: 0, args: s.split(' ').map(|a| Bytes::copy_from_slice(a.as_bytes())).collect() };
        r.apply(&mk("HSET k a 1 b 2"));
        r.apply(&mk("HPEXPIREAT k 1 FIELDS 1 a"));
        let mut rc = Client::new(2);
        assert_eq!(run(&mut r, &mut rc, "HLEN k"), Reply::Int(2));
        assert_eq!(run(&mut r, &mut rc, "HGET k a"), Reply::Nil);
        assert_eq!(run(&mut r, &mut rc, "HLEN k"), Reply::Int(2));
        r.apply(&mk("HDEL k a"));
        assert_eq!(run(&mut r, &mut rc, "HLEN k"), Reply::Int(1));
    }

    #[test]
    fn memory_is_tracked() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        let base = e.used_memory();
        for i in 0..300 {
            run(&mut e, &mut c, &format!("HSET h field:{i} value-{i}-xxxxxxxxxxxxxxxxxxxxxxxx"));
        }
        run(&mut e, &mut c, "HEXPIRE h 1000 FIELDS 2 field:1 field:2");
        assert!(e.used_memory() > base + 300 * 40);
        run(&mut e, &mut c, "DEL h");
        // Only the (empty) hash tables remain.
        assert_eq!(e.dbs[0].mem, 0);
    }
}
