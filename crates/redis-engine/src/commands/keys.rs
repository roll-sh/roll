//! Generic keyspace commands.

use super::*;
use crate::engine::Engine;
use crate::notify;
use crate::util::{fmt_i64, glob_match};
use crate::value::{Kind, Str};
use crate::{cmd, container};

pub static COMMANDS: &[Command] = &[
    cmd!("del", -2, WRITE, 1, -1, 1, del, Generic),
    cmd!("unlink", -2, WRITE | FAST, 1, -1, 1, del, Generic),
    cmd!("exists", -2, READONLY | FAST, 1, -1, 1, exists, Generic),
    cmd!("type", 2, READONLY | FAST, 1, 1, 1, type_, Generic),
    cmd!("expire", -3, WRITE | FAST, 1, 1, 1, expire, Generic),
    cmd!("pexpire", -3, WRITE | FAST, 1, 1, 1, pexpire, Generic),
    cmd!("expireat", -3, WRITE | FAST, 1, 1, 1, expireat, Generic),
    cmd!("pexpireat", -3, WRITE | FAST, 1, 1, 1, pexpireat, Generic),
    cmd!("ttl", 2, READONLY | FAST, 1, 1, 1, ttl, Generic),
    cmd!("pttl", 2, READONLY | FAST, 1, 1, 1, pttl, Generic),
    cmd!("expiretime", 2, READONLY | FAST, 1, 1, 1, expiretime, Generic),
    cmd!("pexpiretime", 2, READONLY | FAST, 1, 1, 1, pexpiretime, Generic),
    cmd!("persist", 2, WRITE | FAST, 1, 1, 1, persist, Generic),
    cmd!("keys", 2, READONLY, 0, 0, 0, keys, Generic),
    cmd!("scan", -2, READONLY, 0, 0, 0, scan, Generic),
    cmd!("randomkey", 1, READONLY, 0, 0, 0, randomkey, Generic),
    cmd!("rename", 3, WRITE, 1, 2, 1, rename, Generic),
    cmd!("renamenx", 3, WRITE | FAST, 1, 2, 1, renamenx, Generic),
    cmd!("move", 3, WRITE | FAST, 1, 1, 1, move_, Generic),
    cmd!("copy", -3, WRITE | DENYOOM, 1, 2, 1, copy, Generic),
    cmd!("touch", -2, READONLY | FAST, 1, -1, 1, touch, Generic),
    cmd!("dump", 2, READONLY, 1, 1, 1, dump, Generic),
    cmd!("restore", -4, WRITE | DENYOOM, 1, 1, 1, restore, Generic),
    Command { since: "3.0.0", ..cmd!("restore-asking", -4, WRITE | DENYOOM | ASKING, 1, 1, 1, restore, Generic) },
    Command { since: "8.0.0", ..cmd!("trimslots", -5, WRITE, 0, 0, 0, super::server::cluster_disabled, Generic) },
    container!("object", 0, Generic, OBJECT),
];

static OBJECT: &[Command] = &[
    cmd!("encoding", 3, READONLY, 2, 2, 1, object_encoding, Generic),
    cmd!("freq", 3, READONLY, 2, 2, 1, object_freq, Generic),
    cmd!("idletime", 3, READONLY, 2, 2, 1, object_idletime, Generic),
    cmd!("refcount", 3, READONLY, 2, 2, 1, object_refcount, Generic),
    cmd!("help", 2, STALE | LOADING, 0, 0, 0, object_help, Generic),
];

fn b(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

pub fn del(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut n = 0;
    for k in &args[1..] {
        if cx.delete(k) {
            cx.notify(notify::GENERIC, "del", k);
            n += 1;
        }
    }
    Ok(Reply::Int(n))
}

pub fn exists(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut n = 0;
    for k in &args[1..] {
        if cx.get_notouch(k).is_some() {
            n += 1;
        }
    }
    Ok(Reply::Int(n))
}

pub fn type_(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::status(cx.get_notouch(&args[1]).map_or("none", |e| e.value.kind().name())))
}

#[derive(Clone, Copy, PartialEq)]
enum ExpireFlag {
    None,
    Nx,
    Xx,
    Gt,
    Lt,
}

/// EXPIRE / PEXPIRE / EXPIREAT / PEXPIREAT key value [NX | XX | GT | LT]
fn expire_generic(cx: &mut Cx, args: &[Bytes], secs: bool, absolute: bool) -> CmdResult {
    let mut flag = ExpireFlag::None;
    for a in &args[3..] {
        let f = if eq(a, "nx") {
            ExpireFlag::Nx
        } else if eq(a, "xx") {
            ExpireFlag::Xx
        } else if eq(a, "gt") {
            ExpireFlag::Gt
        } else if eq(a, "lt") {
            ExpireFlag::Lt
        } else {
            return Err(Error::msg(format!("ERR Unsupported option {}", String::from_utf8_lossy(a))));
        };
        if flag != ExpireFlag::None && flag != f {
            let both = |x, y| (flag == x && f == y) || (flag == y && f == x);
            if both(ExpireFlag::Nx, ExpireFlag::Xx) || both(ExpireFlag::Nx, ExpireFlag::Gt) || both(ExpireFlag::Nx, ExpireFlag::Lt) {
                return Err(Error::new("ERR NX and XX, GT or LT options at the same time are not compatible"));
            }
            if both(ExpireFlag::Gt, ExpireFlag::Lt) {
                return Err(Error::new("ERR GT and LT options at the same time are not compatible"));
            }
            // XX combined with GT/LT is allowed: keep the comparison flag, XX is implied.
            if f == ExpireFlag::Xx {
                continue;
            }
        }
        flag = f;
    }
    let name = String::from_utf8_lossy(&args[0]).to_lowercase();
    let invalid = || Error::msg(format!("ERR invalid expire time in '{name}' command"));
    let v = int(&args[2])?;
    let mut when = v;
    if secs {
        when = when.checked_mul(1000).ok_or_else(invalid)?;
    }
    if !absolute {
        when = when.checked_add(cx.now).ok_or_else(invalid)?;
    }
    let key = &args[1];
    let Some(cur) = cx.expire_of(key) else { return Ok(Reply::Int(0)) };
    let ok = match flag {
        ExpireFlag::None => true,
        ExpireFlag::Nx => cur == 0,
        ExpireFlag::Xx => cur != 0,
        // A key without TTL counts as an infinite TTL.
        ExpireFlag::Gt => cur != 0 && when > cur,
        ExpireFlag::Lt => cur == 0 || when < cur,
    };
    if !ok {
        return Ok(Reply::Int(0));
    }
    if when <= cx.now && !cx.eng.replica && cx.mode != crate::engine::Mode::Replay {
        // Expiring in the past deletes the key.
        cx.delete(key);
        cx.notify(notify::GENERIC, "del", key);
        cx.propagate_as(vec![b("DEL"), key.clone()]);
        return Ok(Reply::Int(1));
    }
    cx.set_expire(key, when);
    cx.notify(notify::GENERIC, "expire", key);
    cx.propagate_as(vec![b("PEXPIREAT"), key.clone(), Bytes::from(fmt_i64(when))]);
    Ok(Reply::Int(1))
}

pub fn expire(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    expire_generic(cx, args, true, false)
}
pub fn pexpire(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    expire_generic(cx, args, false, false)
}
pub fn expireat(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    expire_generic(cx, args, true, true)
}
pub fn pexpireat(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    expire_generic(cx, args, false, true)
}

fn ttl_generic(cx: &mut Cx, key: &[u8], ms: bool) -> CmdResult {
    // lookupKeyReadWithFlags(LOOKUP_NOTOUCH): counted, the access time left alone.
    let Some(e) = cx.get_notouch(key) else { return Ok(Reply::Int(-2)) };
    if e.expire == 0 {
        return Ok(Reply::Int(-1));
    }
    let left = (e.expire - cx.now).max(0);
    Ok(Reply::Int(if ms { left } else { (left + 500) / 1000 }))
}

pub fn ttl(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    ttl_generic(cx, &args[1], false)
}
pub fn pttl(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    ttl_generic(cx, &args[1], true)
}

fn expiretime_generic(cx: &mut Cx, key: &[u8], ms: bool) -> CmdResult {
    let Some(e) = cx.get_notouch(key) else { return Ok(Reply::Int(-2)) };
    if e.expire == 0 {
        return Ok(Reply::Int(-1));
    }
    Ok(Reply::Int(if ms { e.expire } else { e.expire / 1000 }))
}

pub fn expiretime(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    expiretime_generic(cx, &args[1], false)
}
pub fn pexpiretime(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    expiretime_generic(cx, &args[1], true)
}

pub fn persist(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    match cx.expire_of(&args[1]) {
        Some(at) if at != 0 => {
            cx.set_expire(&args[1], 0);
            cx.notify(notify::GENERIC, "persist", &args[1]);
            Ok(Reply::Int(1))
        }
        _ => Ok(Reply::Int(0)),
    }
}

pub fn keys(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let pat = &args[1];
    let all = pat.as_ref() == b"*";
    let now = cx.eng.expiry_clock(cx.now);
    let budget = cx.reply_budget();
    let mut out = Vec::new();
    let mut bytes = 0u64;
    // Expired keys are skipped, not deleted (`keyIsExpired`).
    for (k, e) in cx.db().keys.iter() {
        if (all || glob_match(pat, k, false)) && !e.expired(now) {
            out.push(Reply::bulk(k));
            bytes += k.len() as u64 + 5;
        }
        // Past the output buffer limit the client is closed: stop producing.
        if bytes > budget {
            break;
        }
    }
    Ok(Reply::Array(out))
}

/// SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]
pub fn scan(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let cursor = parse_cursor(&args[1])?;
    let opts = ScanOpts::parse(&args[2..], true)?;
    let (next, keys) = scan_db(cx, cursor, &opts);
    Ok(scan_reply(next, keys.into_iter().map(Reply::bulk).collect()))
}

pub fn parse_cursor(a: &[u8]) -> Result<u64, Error> {
    crate::util::parse_u64(a).ok_or(Error::new("ERR invalid cursor"))
}

pub fn scan_reply(cursor: u64, items: Vec<Reply>) -> Reply {
    Reply::Array(vec![Reply::bulk_str(cursor.to_string()), Reply::Array(items)])
}

pub struct ScanOpts {
    pub pattern: Option<Bytes>,
    pub count: usize,
    pub kind: Option<Kind>,
    /// Type name given but unknown: return nothing (Redis 7.x behaviour).
    pub bad_type: bool,
    pub novalues: bool,
}

impl ScanOpts {
    pub fn parse(args: &[Bytes], allow_type: bool) -> Result<Self, Error> {
        let mut o = ScanOpts { pattern: None, count: 10, kind: None, bad_type: false, novalues: false };
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if eq(a, "count") && i + 1 < args.len() {
                let c = int(&args[i + 1])?;
                if c < 1 {
                    return Err(SYNTAX);
                }
                o.count = c as usize;
                i += 2;
            } else if eq(a, "match") && i + 1 < args.len() {
                let p = args[i + 1].clone();
                o.pattern = if p.as_ref() == b"*" { None } else { Some(p) };
                i += 2;
            } else if allow_type && eq(a, "type") && i + 1 < args.len() {
                match Kind::parse(&args[i + 1]) {
                    Some(k) => o.kind = Some(k),
                    // Redis 8 returns no keys for an unknown type name.
                    None => o.bad_type = true,
                }
                i += 2;
            } else if !allow_type && eq(a, "novalues") {
                o.novalues = true;
                i += 1;
            } else {
                return Err(SYNTAX);
            }
        }
        Ok(o)
    }

    pub fn matches(&self, k: &[u8]) -> bool {
        self.pattern.as_ref().is_none_or(|p| glob_match(p, k, false))
    }
}

/// One SCAN step over the current database: `(next cursor, keys)`.
pub fn scan_db(cx: &mut Cx, cursor: u64, o: &ScanOpts) -> (u64, Vec<Str>) {
    let now = cx.eng.expiry_clock(cx.now);
    let db = cx.db();
    let mut found: Vec<(Str, bool, Kind)> = Vec::new();
    let mut c = cursor;
    let mut iterations = o.count * 10;
    loop {
        c = db.keys.scan(c, |k, e| found.push((k.clone(), e.expired(now), e.value.kind())));
        iterations -= 1;
        if c == 0 || iterations == 0 || found.len() >= o.count {
            break;
        }
    }
    let mut out = Vec::with_capacity(found.len());
    for (k, expired, kind) in found {
        if expired {
            cx.peek(&k);
            continue;
        }
        if o.bad_type || o.kind.is_some_and(|t| t != kind) || !o.matches(&k) {
            continue;
        }
        out.push(k);
    }
    (c, out)
}

pub fn randomkey(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let now = cx.eng.expiry_clock(cx.now);
    for _ in 0..100 {
        let Engine { dbs, rng, .. } = &mut *cx.eng;
        let db = &dbs[cx.client.db];
        let Some((k, e)) = db.keys.random(rng) else { return Ok(Reply::Nil) };
        let (k, expired) = (k.clone(), e.expired(now));
        if expired && !cx.eng.replica {
            cx.peek(&k);
            continue;
        }
        return Ok(Reply::bulk(k));
    }
    Ok(Reply::Nil)
}

fn rename_generic(cx: &mut Cx, args: &[Bytes], nx: bool) -> CmdResult {
    let (src, dst) = (&args[1], &args[2]);
    if cx.peek(src).is_none() {
        return Err(NO_KEY);
    }
    if src == dst {
        return Ok(if nx { Reply::Int(0) } else { Reply::Ok });
    }
    if nx && cx.peek(dst).is_some() {
        return Ok(Reply::Int(0));
    }
    let mut entry = cx.take(src).unwrap();
    entry.rekey(src.len(), dst.len(), false);
    let db = cx.db_index();
    let src_kind = entry.value.kind();
    let dst_kind = cx.peek(dst).map(|e| e.value.kind());
    if dst_kind.is_some() {
        cx.delete(dst);
    }
    cx.put_entry(db, dst, entry);
    cx.changes(1);
    cx.notify(notify::GENERIC, "rename_from", src);
    cx.notify(notify::GENERIC, "rename_to", dst);
    if let Some(k) = dst_kind {
        overwritten(cx, db, dst, k, src_kind);
    }
    Ok(if nx { Reply::Int(1) } else { Reply::Ok })
}

pub fn rename(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    rename_generic(cx, args, false)
}
pub fn renamenx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    rename_generic(cx, args, true)
}

fn db_index(cx: &Cx, a: &[u8]) -> Result<usize, Error> {
    let n = crate::util::parse_i64(a).ok_or(Error::new("ERR value is not an integer or out of range"))?;
    if n < 0 || n as usize >= cx.eng.dbs.len() {
        return Err(Error::new("ERR DB index is out of range"));
    }
    Ok(n as usize)
}

pub fn move_(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let to = db_index(cx, &args[2])?;
    let from = cx.db_index();
    if from == to {
        return Err(Error::new("ERR source and destination objects are the same"));
    }
    let key = &args[1];
    if cx.peek(key).is_none() {
        return Ok(Reply::Int(0));
    }
    let now = cx.eng.expiry_clock(cx.now);
    if let Some(e) = cx.eng.dbs[to].keys.get(key) {
        if !e.expired(now) {
            return Ok(Reply::Int(0));
        }
        cx.eng.delete_expired(to, key);
    }
    let mut entry = cx.take(key).unwrap();
    entry.rekey(key.len(), key.len(), false);
    cx.put_entry(to, key, entry);
    cx.changes(1);
    cx.notify(notify::GENERIC, "move_from", key);
    cx.eng.sig.notify(notify::GENERIC, "move_to", to as u16, key);
    Ok(Reply::Int(1))
}

/// COPY source destination [DB destination-db] [REPLACE]
pub fn copy(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut to = cx.db_index();
    let mut replace = false;
    let mut i = 3;
    while i < args.len() {
        if eq(&args[i], "replace") {
            replace = true;
            i += 1;
        } else if eq(&args[i], "db") && i + 1 < args.len() {
            to = db_index(cx, &args[i + 1])?;
            i += 2;
        } else {
            return Err(SYNTAX);
        }
    }
    let (src, dst) = (&args[1], &args[2]);
    let from = cx.db_index();
    if from == to && src == dst {
        return Err(Error::new("ERR source and destination objects are the same"));
    }
    // lookupKeyRead.
    let Some(entry) = cx.get_entry(src).cloned() else { return Ok(Reply::Int(0)) };
    let now = cx.eng.expiry_clock(cx.now);
    let exists = cx.eng.dbs[to].keys.get(dst).is_some_and(|e| !e.expired(now));
    if exists && !replace {
        return Ok(Reply::Int(0));
    }
    let old_kind = cx.eng.dbs[to].keys.get(dst).map(|e| e.value.kind());
    if old_kind.is_some() {
        cx.eng.dbs[to].remove(dst);
        cx.eng.sig.touch(to as u16, dst);
    }
    // Module types without a copy callback.
    if matches!(entry.value, crate::value::Value::Sketch(_) | crate::value::Value::VSet(_) | crate::value::Value::Sug(_)) {
        return Err(Error::new("ERR not supported for this module key"));
    }
    let mut entry = entry;
    // RedisTimeSeries' `CopySeries`: no links to other series, named after the copy, indexed.
    let ts_labels = match &mut entry.value {
        crate::value::Value::Ts(t) => {
            **t = crate::commands::ts::copy_series(t, dst);
            Some(t.labels.clone())
        }
        _ => None,
    };
    entry.lru = cx.eng.new_lru(now);
    entry.rekey(src.len(), dst.len(), true);
    let new_kind = entry.value.kind();
    cx.put_entry(to, dst, entry);
    if let Some(labels) = ts_labels {
        cx.eng.ts.index.remove(dst);
        cx.eng.ts.index.add(dst, &labels);
    }
    cx.changes(1);
    cx.eng.sig.notify(notify::GENERIC, "copy_to", to as u16, dst);
    if let Some(k) = old_kind.filter(|_| exists) {
        overwritten(cx, to, dst, k, new_kind);
    }
    Ok(Reply::Int(1))
}

pub fn touch(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut n = 0;
    for k in &args[1..] {
        if cx.get(k).is_some() {
            n += 1;
        }
    }
    Ok(Reply::Int(n))
}

/// `DUMP key`: the value in RDB format (see [`crate::rdb::dump`]).
pub fn dump(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let now = cx.now;
    if cx.get(&args[1]).is_none() {
        return Ok(Reply::Nil);
    }
    let db = cx.db_index();
    let e = cx.eng.dbs[db].keys.get_mut(&args[1]).expect("found");
    e.value.prepare_save();
    Ok(Reply::Bulk(Bytes::from(crate::rdb::dump(&e.value, now))))
}

/// `RESTORE key ttl payload [REPLACE] [ABSTTL] [IDLETIME s] [FREQ f]` (`restoreCommand`).
pub fn restore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (mut replace, mut absttl) = (false, false);
    let (mut idle, mut freq) = (None::<u64>, None::<u8>);
    let mut j = 4;
    while j < args.len() {
        let more = j + 1 < args.len();
        if eq(&args[j], "replace") {
            replace = true;
        } else if eq(&args[j], "absttl") {
            absttl = true;
        } else if eq(&args[j], "idletime") && more && freq.is_none() {
            let v = int(&args[j + 1])?;
            if v < 0 {
                return Err(Error::new("ERR Invalid IDLETIME value, must be >= 0"));
            }
            idle = Some(v as u64);
            j += 1;
        } else if eq(&args[j], "freq") && more && idle.is_none() {
            let v = int(&args[j + 1])?;
            if !(0..=255).contains(&v) {
                return Err(Error::new("ERR Invalid FREQ value, must be >= 0 and <= 255"));
            }
            freq = Some(v as u8);
            j += 1;
        } else {
            return Err(SYNTAX);
        }
        j += 1;
    }
    let key = &args[1];
    if !replace && cx.peek(key).is_some() {
        return Err(Error::new("BUSYKEY Target key name already exists."));
    }
    let ttl = int(&args[2])?;
    if ttl < 0 {
        return Err(Error::new("ERR Invalid TTL value, must be >= 0"));
    }
    let now = cx.now;
    let reg = cx.eng.hub.templates.clone();
    let value = match crate::rdb::restore_with(&args[3], cx.cfg(), &reg, now, cx.eng.debug.skip_checksum) {
        Ok(v) => v,
        Err(crate::rdb::RdbError::Version(_) | crate::rdb::RdbError::Checksum) => {
            return Err(Error::new("ERR DUMP payload version or checksum are wrong"));
        }
        Err(_) => return Err(Error::new("ERR Bad data format")),
    };
    if matches!(value, crate::value::Value::Ts(_)) {
        // Loading a series subscribes RedisTimeSeries to keyspace events (`NewSeries`).
        cx.eng.sig.ts_hook = true;
    }
    let at = if ttl != 0 && !absttl { ttl.wrapping_add(now) } else { ttl };
    let old_kind = cx.peek(key).map(|e| e.value.kind()).filter(|_| replace);
    let deleted = replace && cx.delete(key);
    if at != 0 && at <= now && !cx.eng.replica && cx.mode != crate::engine::Mode::Replay {
        // Already expired: nothing is created; a replaced key is deleted.
        if deleted {
            let del = if cx.cfg().lazyfree_lazy_server_del { "UNLINK" } else { "DEL" };
            cx.propagate_as(vec![b(del), key.clone()]);
            cx.notify(notify::GENERIC, "del", key);
        }
        cx.eng.stats.expired_keys += 1;
        return Ok(Reply::Ok);
    }
    let lru = Engine::restored_lru(cx.cfg(), freq, idle, now);
    let db = cx.db_index();
    cx.put_entry(db, key, crate::value::Entry::with_expire(value, at, lru));
    cx.changes(1);
    if ttl != 0 && !absttl {
        // Replicate the absolute expiry.
        let mut argv = args.to_vec();
        argv[2] = Bytes::from(fmt_i64(at));
        argv.push(b("ABSTTL"));
        cx.propagate_as(argv);
    }
    cx.notify(notify::GENERIC, "restore", key);
    if let Some(k) = old_kind {
        let new_kind = cx.peek(key).map_or(k, |e| e.value.kind());
        overwritten(cx, db, key, k, new_kind);
    }
    Ok(Reply::Ok)
}

/// The `overwritten` (and on a type change `type_changed`) events of a replaced key.
fn overwritten(cx: &mut Cx, db: usize, key: &[u8], old: crate::value::Kind, new: crate::value::Kind) {
    cx.eng.sig.notify(notify::OVERWRITTEN, "overwritten", db as u16, key);
    if old != new {
        cx.eng.sig.notify(notify::TYPE_CHANGED, "type_changed", db as u16, key);
    }
}

pub fn object_encoding(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(match cx.get_object(&args[2]) {
        Some(e) => Reply::bulk(e.encoding(args[2].len())),
        None => Reply::Nil,
    })
}

pub fn object_freq(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let now = cx.now;
    if cx.get_object(&args[2]).is_none() {
        return Ok(Reply::Nil);
    }
    if !cx.cfg().maxmemory_policy.is_lfu() {
        return Err(Error::new("ERR An LFU maxmemory policy is not selected, access frequency not tracked. Please note that when switching between policies at runtime LRU and LFU data will take some time to adjust."));
    }
    let e = cx.eng.dbs[cx.client.db].keys.get(&args[2]).expect("looked up");
    Ok(Reply::Int(cx.eng.lfu_counter(e, now) as i64))
}

pub fn object_idletime(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let now = cx.now;
    let Some(idle) = cx.get_object(&args[2]).map(|e| Engine::idle_secs(e, now)) else { return Ok(Reply::Nil) };
    if cx.cfg().maxmemory_policy.is_lfu() {
        return Err(Error::new("ERR An LFU maxmemory policy is selected, idle time not tracked. Please note that when switching between policies at runtime LRU and LFU data will take some time to adjust."));
    }
    Ok(Reply::Int(idle as i64))
}

pub fn object_refcount(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    // A key's value is its kvobj (the key embedded), never a shared object.
    Ok(match cx.get_object(&args[2]) {
        Some(_) => Reply::Int(1),
        None => Reply::Nil,
    })
}

pub fn object_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "OBJECT <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "ENCODING <key>",
        "    Return the kind of internal representation used in order to store the value",
        "    associated with a <key>.",
        "FREQ <key>",
        "    Return the access frequency index of the <key>. The returned integer is",
        "    proportional to the logarithm of the recent access frequency of the key.",
        "IDLETIME <key>",
        "    Return the idle time of the <key>, that is the approximated number of",
        "    seconds elapsed since the last access to the key.",
        "REFCOUNT <key>",
        "    Return the number of references of the value associated with the specified",
        "    <key>.",
        "HELP",
        "    Print this help.",
    ]))
}
