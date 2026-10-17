//! Stream commands: XADD, XRANGE, XREAD, consumer groups (XGROUP, XREADGROUP, XACK, XCLAIM,
//! XAUTOCLAIM, XPENDING, XNACK), XINFO, trimming/deletion (XTRIM, XDEL, XDELEX, XACKDEL),
//! XSETID and idempotent producers (IDMP, XCFGSET, XIDMPRECORD).
//!
//! Argument parsing, error precedence, notifications and propagation follow `t_stream.c`:
//! XADD with `*` / `ms-*` propagates the generated ID and `~` trims propagate as `=` with the
//! resulting length / first ID; XREADGROUP, XCLAIM and XAUTOCLAIM propagate one `XCLAIM … FORCE
//! JUSTID` per delivered entry (plus `XGROUP SETID`), never themselves. Group bookkeeping that
//! Redis applies with `keyModified(…, 0)` changes the key without invalidating WATCH.

use super::*;
use crate::engine::{Effect, Mode};
use crate::notify;
use crate::types::stream::{Entries, EntryRef, Group, RefPolicy, Stream, StreamId, Trim, TrimBy};
use crate::util::{fmt_i64, parse_i64};
use crate::value::{Kind, Str};
use crate::{cmd, container};
use bytes::BytesMut;

pub static COMMANDS: &[Command] = &[
    cmd!("xadd", -5, WRITE | DENYOOM | FAST, 1, 1, 1, xadd, Stream),
    cmd!("xrange", -4, READONLY, 1, 1, 1, xrange, Stream),
    cmd!("xrevrange", -4, READONLY, 1, 1, 1, xrevrange, Stream),
    cmd!("xlen", 2, READONLY | FAST, 1, 1, 1, xlen, Stream),
    cmd!("xread", -4, READONLY | BLOCKING, keys = xread_keys, xread, Stream),
    cmd!("xreadgroup", -7, WRITE | BLOCKING, keys = xread_keys, xreadgroup, Stream),
    cmd!("xdel", -3, WRITE | FAST, 1, 1, 1, xdel, Stream),
    cmd!("xdelex", -5, WRITE | FAST, 1, 1, 1, xdelex, Stream),
    cmd!("xtrim", -4, WRITE, 1, 1, 1, xtrim, Stream),
    cmd!("xack", -4, WRITE | FAST, 1, 1, 1, xack, Stream),
    cmd!("xackdel", -6, WRITE | FAST, 1, 1, 1, xackdel, Stream),
    cmd!("xnack", -7, WRITE | FAST, 1, 1, 1, xnack, Stream),
    cmd!("xpending", -3, READONLY, 1, 1, 1, xpending, Stream),
    cmd!("xclaim", -6, WRITE | FAST, 1, 1, 1, xclaim, Stream),
    cmd!("xautoclaim", -6, WRITE | FAST, 1, 1, 1, xautoclaim, Stream),
    cmd!("xsetid", -3, WRITE | DENYOOM | FAST, 1, 1, 1, xsetid, Stream),
    cmd!("xcfgset", -2, WRITE | FAST, 1, 1, 1, xcfgset, Stream),
    cmd!("xidmprecord", 5, WRITE | DENYOOM | FAST, 1, 1, 1, xidmprecord, Stream),
    container!("xgroup", 0, Stream, XGROUP),
    container!("xinfo", 0, Stream, XINFO),
];

static XGROUP: &[Command] = &[
    cmd!("create", -5, WRITE | DENYOOM, 2, 2, 1, xgroup_create, Stream),
    cmd!("destroy", 4, WRITE, 2, 2, 1, xgroup_destroy, Stream),
    cmd!("createconsumer", 5, WRITE | DENYOOM, 2, 2, 1, xgroup_createconsumer, Stream),
    cmd!("setid", -5, WRITE, 2, 2, 1, xgroup_setid, Stream),
    cmd!("delconsumer", 5, WRITE, 2, 2, 1, xgroup_delconsumer, Stream),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, xgroup_help, Stream),
];

static XINFO: &[Command] = &[
    cmd!("stream", -3, READONLY, 2, 2, 1, xinfo_stream, Stream),
    cmd!("groups", 3, READONLY, 2, 2, 1, xinfo_groups, Stream),
    cmd!("consumers", 4, READONLY, 2, 2, 1, xinfo_consumers, Stream),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, xinfo_help, Stream),
];

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

const INVALID_ID: Error = Error::new("ERR Invalid stream ID specified as stream command argument");
const SMALLER_ID: Error = Error::new("ERR The ID specified in XADD is equal or smaller than the target stream top item");
const INVALID_ENTRIES_READ: i64 = -1;

fn b(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

fn num(i: i64) -> Bytes {
    Bytes::from(fmt_i64(i))
}

/// The bytes a C string function would see (up to the first NUL).
fn cstr(a: &[u8]) -> &[u8] {
    match a.iter().position(|&c| c == 0) {
        Some(i) => &a[..i],
        None => a,
    }
}

/// `%s` formatting of an argument in an error message.
fn show(a: &[u8]) -> String {
    String::from_utf8_lossy(cstr(a)).into_owned()
}

fn id_str(id: StreamId) -> Bytes {
    let mut buf = itoa::Buffer::new();
    let mut v = Vec::with_capacity(24);
    v.extend_from_slice(buf.format(id.ms).as_bytes());
    v.push(b'-');
    v.extend_from_slice(buf.format(id.seq).as_bytes());
    Bytes::from(v)
}

fn id_reply(id: StreamId) -> Reply {
    Reply::Bulk(id_str(id))
}

/// `streamIncrID` (wrapping to 0-0 past the maximum).
fn incr_wrap(id: StreamId) -> StreamId {
    id.next().unwrap_or(StreamId::MIN)
}

/// `streamDecrID` (wrapping to the maximum below 0-0).
fn decr_wrap(id: StreamId) -> StreamId {
    id.prev().unwrap_or(StreamId::MAX)
}

/// `strtoull(s, &end, 10)` accepting the whole string: leading whitespace, a sign (negative
/// values wrap), digits; no overflow.
fn strtoull(s: &[u8]) -> Option<u64> {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut v: u64 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        v = v.checked_mul(10)?.checked_add((s[i] - b'0') as u64)?;
        i += 1;
    }
    if i == start || i != s.len() {
        return None;
    }
    Some(if neg { v.wrapping_neg() } else { v })
}

/// Redis' `string2ull`.
fn string2ull(s: &[u8]) -> Option<u64> {
    if let Some(v) = parse_i64(s) {
        return if v < 0 { None } else { Some(v as u64) };
    }
    strtoull(s)
}

/// `streamGenericParseIDOrReply`: returns the ID and whether the sequence was given (false only
/// for `<ms>-*` when `seq_star` allows it).
fn parse_id_generic(a: &[u8], missing_seq: u64, strict: bool, seq_star: bool) -> Result<(StreamId, bool), Error> {
    if a.len() > 127 {
        return Err(INVALID_ID);
    }
    let s = cstr(a);
    if strict && (s == b"-" || s == b"+") {
        return Err(INVALID_ID);
    }
    if s == b"-" {
        return Ok((StreamId::MIN, true));
    }
    if s == b"+" {
        return Ok((StreamId::MAX, true));
    }
    let (ms_part, seq_part) = match s.iter().position(|&c| c == b'-') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let ms = string2ull(ms_part).ok_or(INVALID_ID)?;
    let (seq, given) = match seq_part {
        Some(b"*") if seq_star => (0, false),
        Some(p) => (string2ull(p).ok_or(INVALID_ID)?, true),
        None => (missing_seq, true),
    };
    Ok((StreamId::new(ms, seq), given))
}

/// A strict ID (`-` and `+` rejected).
fn parse_strict(a: &[u8], missing_seq: u64) -> Result<StreamId, Error> {
    Ok(parse_id_generic(a, missing_seq, true, false)?.0)
}

/// A non-strict ID (`-` and `+` accepted).
fn parse_loose(a: &[u8], missing_seq: u64) -> Result<StreamId, Error> {
    Ok(parse_id_generic(a, missing_seq, false, false)?.0)
}

/// `streamParseIntervalIDOrReply`: `(` makes the bound exclusive.
fn parse_interval(a: &[u8], missing_seq: u64) -> Result<(StreamId, bool), Error> {
    if a.len() > 1 && a[0] == b'(' {
        Ok((parse_strict(&a[1..], missing_seq)?, true))
    } else {
        Ok((parse_loose(a, missing_seq)?, false))
    }
}

fn range_start(a: &[u8]) -> Result<StreamId, Error> {
    let (id, ex) = parse_interval(a, 0)?;
    if ex {
        return id.next().ok_or(Error::new("ERR invalid start ID for the interval"));
    }
    Ok(id)
}

fn range_end(a: &[u8]) -> Result<StreamId, Error> {
    let (id, ex) = parse_interval(a, u64::MAX)?;
    if ex {
        return id.prev().ok_or(Error::new("ERR invalid end ID for the interval"));
    }
    Ok(id)
}

fn fields_reply(e: &EntryRef) -> Reply {
    let mut fv = Vec::with_capacity(e.len() * 2);
    for (f, v) in e.fields() {
        fv.push(Reply::bulk(f));
        fv.push(Reply::bulk(v));
    }
    Reply::Array(fv)
}

/// `[id, [field, value, …]]`.
fn entry_reply(e: &EntryRef) -> Reply {
    Reply::Array(vec![id_reply(e.id), fields_reply(e)])
}

/// Encoded size of a reply (for MAXSIZE).
fn rsize(r: &Reply, resp: u8) -> u64 {
    let mut buf = BytesMut::new();
    r.encode(resp, &mut buf);
    buf.len() as u64
}

/// Size of an aggregate header (`*<n>\r\n`).
fn hdr_size(n: usize) -> u64 {
    3 + itoa::Buffer::new().format(n).len() as u64
}

fn bulk_size(b: &[u8]) -> u64 {
    hdr_size(b.len()) + b.len() as u64 + 2
}

/// Count a write that doesn't signal the key (Redis' `keyModified(…, 0)` + `server.dirty++`):
/// drives propagation of the command without invalidating WATCH or waking blocked clients.
fn dirty(cx: &mut Cx) {
    cx.eng.sig.dirty += 1;
}

/// Propagate an effect now (skipped when replaying, where effects aren't recorded).
fn prop(cx: &mut Cx, args: Vec<Bytes>) {
    if cx.mode != Mode::Replay {
        let db = cx.db_index();
        cx.also_propagate(db, args);
    }
}

/// Wrap several effects of one top-level command in MULTI/EXEC.
fn wrap(cx: &mut Cx, start: usize) {
    if matches!(cx.mode, Mode::Top | Mode::Unblock) {
        let n = cx.eng.effects.len() - start;
        if n > 1 {
            let db = cx.eng.effects[start].db;
            cx.eng.effects.insert(start, Effect { db, args: vec![b("MULTI")] });
            cx.eng.effects.push(Effect { db, args: vec![b("EXEC")] });
        }
    }
}

fn nogroup(key: &[u8], group: &[u8]) -> Error {
    Error::msg(format!("NOGROUP No such key '{}' or consumer group '{}'", show(key), show(group)))
}

/// `XCLAIM <key> <group> <consumer> 0 <id> TIME <ms> RETRYCOUNT <n> FORCE JUSTID LASTID <id>`.
#[allow(clippy::too_many_arguments)]
fn xclaim_effect(key: &Bytes, group: &Bytes, consumer: &[u8], id: Bytes, time: i64, count: u64, last: &Bytes) -> Vec<Bytes> {
    vec![
        b("XCLAIM"),
        key.clone(),
        group.clone(),
        Bytes::copy_from_slice(consumer),
        b("0"),
        id,
        b("TIME"),
        num(time),
        b("RETRYCOUNT"),
        num(count as i64),
        b("FORCE"),
        b("JUSTID"),
        b("LASTID"),
        last.clone(),
    ]
}

fn setid_effect(key: &Bytes, group: &Bytes, g: &Group) -> Vec<Bytes> {
    vec![b("XGROUP"), b("SETID"), key.clone(), group.clone(), id_str(g.last_id), b("ENTRIESREAD"), num(g.entries_read)]
}

/// IIDs recorded at or before this ms have expired.
fn idmp_cutoff(now: i64, s: &Stream) -> u64 {
    (now as u64).wrapping_sub(s.idmp_duration.wrapping_mul(1000))
}

// ---------------------------------------------------------------------------------------------
// XADD / XTRIM
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct AddTrimArgs {
    by: Option<TrimBy>,
    approx: bool,
    limit: i64,
    policy: Option<RefPolicy>,
    /// Index of the MAXLEN/MINID value.
    strategy_idx: usize,
    nomkstream: bool,
    idmp_pid: Option<usize>,
    idmp_iid: Option<usize>,
    idmp_auto: bool,
    id: StreamId,
    id_given: bool,
    seq_given: bool,
}

impl AddTrimArgs {
    fn trim(&self) -> Option<Trim> {
        self.by.map(|by| Trim { by, approx: self.approx, limit: self.limit.max(0) as u64, policy: self.policy.unwrap_or_default() })
    }
}

/// `streamParseAddOrTrimArgsOrReply`; returns the args and the ID position (XADD).
fn parse_add_trim(cx: &Cx, args: &[Bytes], xadd: bool) -> Result<(AddTrimArgs, usize), Error> {
    let mut a = AddTrimArgs::default();
    let argc = args.len();
    let mut limit_given = false;
    let mut i = 2;
    while i < argc {
        let more = argc - 1 - i;
        let opt = &args[i];
        if xadd && cstr(opt) == b"*" {
            break;
        } else if (eq(opt, "maxlen") || eq(opt, "minid")) && more > 0 {
            if a.by.is_some() {
                return Err(Error::new("ERR syntax error, MAXLEN and MINID options at the same time are not compatible"));
            }
            a.approx = false;
            let next = cstr(&args[i + 1]);
            if more >= 2 && next == b"~" {
                a.approx = true;
                i += 1;
            } else if more >= 2 && next == b"=" {
                i += 1;
            }
            if eq(opt, "maxlen") {
                let n = int(&args[i + 1])?;
                if n < 0 {
                    return Err(Error::new("ERR The MAXLEN argument must be >= 0."));
                }
                a.by = Some(TrimBy::MaxLen(n as u64));
            } else {
                a.by = Some(TrimBy::MinId(parse_strict(&args[i + 1], 0)?));
            }
            i += 1;
            a.strategy_idx = i;
        } else if eq(opt, "limit") && more > 0 {
            a.limit = int(&args[i + 1])?;
            if a.limit < 0 {
                return Err(Error::new("ERR The LIMIT argument must be >= 0."));
            }
            limit_given = true;
            i += 1;
        } else if eq(opt, "keepref") && a.policy.is_none() {
            a.policy = Some(RefPolicy::KeepRef);
        } else if eq(opt, "delref") && a.policy.is_none() {
            a.policy = Some(RefPolicy::DelRef);
        } else if eq(opt, "acked") && a.policy.is_none() {
            a.policy = Some(RefPolicy::Acked);
        } else if xadd && eq(opt, "nomkstream") {
            a.nomkstream = true;
        } else if xadd && eq(opt, "idmpauto") && more > 0 {
            if a.idmp_pid.is_some() {
                return Err(Error::new("ERR syntax error, IDMP/IDMPAUTO specified multiple times"));
            }
            if args[i + 1].is_empty() {
                return Err(Error::new("ERR syntax error, IDMPAUTO requires a non-empty producer ID"));
            }
            a.idmp_pid = Some(i + 1);
            a.idmp_auto = true;
            i += 1;
        } else if xadd && eq(opt, "idmp") && more >= 2 {
            if a.idmp_pid.is_some() {
                return Err(Error::new("ERR syntax error, IDMP/IDMPAUTO specified multiple times"));
            }
            if args[i + 1].is_empty() {
                return Err(Error::new("ERR syntax error, IDMP requires a non-empty producer ID"));
            }
            if args[i + 2].is_empty() {
                return Err(Error::new("ERR syntax error, IDMP requires a non-empty idempotent ID"));
            }
            a.idmp_pid = Some(i + 1);
            a.idmp_iid = Some(i + 2);
            i += 2;
        } else if xadd {
            let (id, seq_given) = parse_id_generic(opt, 0, true, true)?;
            if a.idmp_pid.is_some() && opt.first() != Some(&b'*') && cx.mode != Mode::Replay {
                return Err(Error::new("ERR syntax error, IDMP/IDMPAUTO can be used only with auto-generated IDs"));
            }
            a.id = id;
            a.seq_given = seq_given;
            a.id_given = true;
            break;
        } else {
            return Err(SYNTAX);
        }
        i += 1;
    }
    if a.limit != 0 && a.by.is_none() {
        return Err(Error::new("ERR syntax error, LIMIT cannot be used without specifying a trimming strategy"));
    }
    if !xadd && a.by.is_none() {
        return Err(Error::new("ERR syntax error, XTRIM must be called with a trimming strategy"));
    }
    if cx.mode == Mode::Replay {
        a.limit = 0;
    } else if limit_given {
        if !a.approx {
            return Err(Error::new("ERR syntax error, LIMIT cannot be used without the special ~ option"));
        }
    } else if a.approx {
        let mut l = 100i64.saturating_mul(cx.cfg().stream_node_max_entries as i64);
        if l <= 0 {
            l = 10000;
        }
        a.limit = l.min(1_000_000);
    } else {
        a.limit = 0;
    }
    Ok((a, i))
}

/// The `=`-rewrite of an approximate trim: the resulting length, or the first ID (tombstones
/// included).
fn trim_rewrite(s: &Stream, by: TrimBy) -> Bytes {
    match by {
        TrimBy::MaxLen(_) => num(s.entries.len() as i64),
        TrimBy::MinId(_) => id_str(s.entries.first_raw_id()),
    }
}

/// The IDMPAUTO idempotent ID: an order-independent XXH3-128 combination of the field/value
/// pairs (`createIdempotencyHash`), as the 16 bytes of `XXH128_hash_t`.
fn idmp_auto_iid(fv: &[Bytes]) -> Vec<u8> {
    let (mut sum_lo, mut sum_hi, mut xor_lo, mut xor_hi) = (0u64, 0u64, 0u64, 0u64);
    let mut buf = Vec::new();
    for pair in fv.chunks(2) {
        buf.clear();
        buf.extend_from_slice(&pair[0]);
        buf.extend_from_slice(&(pair[0].len() as i64).to_le_bytes());
        buf.extend_from_slice(&pair[1]);
        let h = xxhash_rust::xxh3::xxh3_128(&buf);
        let (lo, hi) = (h as u64, (h >> 64) as u64);
        sum_lo = sum_lo.wrapping_add(lo);
        sum_hi = sum_hi.wrapping_add(hi);
        xor_lo ^= lo;
        xor_hi ^= hi;
    }
    let lo = sum_lo ^ xor_hi.rotate_left(1);
    let hi = sum_hi ^ xor_lo.rotate_left(1);
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&lo.to_le_bytes());
    out.extend_from_slice(&hi.to_le_bytes());
    out
}

pub fn xadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (a, idpos) = parse_add_trim(cx, args, true)?;
    let field_pos = idpos + 1;
    let nfv = args.len() as i64 - field_pos as i64;
    if nfv < 2 || nfv % 2 == 1 {
        return Err(arity_error("xadd"));
    }
    if a.id_given && a.seq_given && a.id.is_zero() {
        return Err(Error::new("ERR The ID specified in XADD must be greater than 0-0"));
    }
    let key = &args[1];
    let exists = cx.check_type(key, Kind::Stream)?;
    if !exists && a.nomkstream {
        return Ok(Reply::Nil);
    }
    let fv = &args[field_pos..];
    let idmp: Option<(&[u8], Vec<u8>)> = a.idmp_pid.map(|p| {
        let iid = match a.idmp_iid {
            Some(i) => args[i].to_vec(),
            None => idmp_auto_iid(fv),
        };
        (&args[p][..], iid)
    });
    let (node_bytes, node_entries) = (cx.cfg().stream_node_max_bytes, cx.cfg().stream_node_max_entries);
    let (dur, maxsize) = (cx.cfg().stream_idmp_duration, cx.cfg().stream_idmp_maxsize);
    let now = cx.now;
    let trim = a.trim();

    let mut km = cx.get_or_create(key, Kind::Stream)?;
    let s = km.stream();
    if !exists {
        s.idmp_duration = dur;
        s.idmp_maxsize = maxsize;
    }
    if let Some((pid, iid)) = &idmp {
        s.idmp.expire(idmp_cutoff(now, s));
        s.idmp.ensure_producer(pid);
        if let Some(found) = s.idmp.lookup(pid, iid) {
            s.iids_duplicates += 1;
            km.unchanged();
            drop(km);
            dirty(cx);
            return Ok(id_reply(found));
        }
    }
    let last = s.entries.last_id();
    if last == StreamId::MAX {
        km.unchanged();
        return Err(Error::new("ERR The stream has exhausted the last possible ID, unable to add more items"));
    }
    let id = if !a.id_given {
        s.entries.next_id(now.max(0) as u64).expect("not exhausted")
    } else if a.seq_given {
        a.id
    } else if last.ms == a.id.ms {
        if last.seq == u64::MAX {
            km.unchanged();
            return Err(SMALLER_ID);
        }
        StreamId::new(last.ms, last.seq + 1)
    } else {
        a.id
    };
    if id <= last {
        km.unchanged();
        return Err(SMALLER_ID);
    }
    s.entries.append(id, fv, node_bytes, node_entries);
    if let Some((pid, iid)) = &idmp {
        let max = s.idmp_maxsize;
        s.idmp.insert(pid, iid, id, max);
        s.iids_added += 1;
    }
    let mut deleted = 0;
    let mut rewrite_trim = None;
    if let Some(t) = &trim {
        deleted = s.trim(t);
        if a.approx {
            rewrite_trim = Some(trim_rewrite(s, t.by));
        }
    }
    drop(km);
    cx.notify(notify::STREAM, "xadd", key);
    if deleted > 0 {
        cx.notify(notify::STREAM, "xtrim", key);
    }
    let auto_id = !a.id_given || !a.seq_given;
    if auto_id || rewrite_trim.is_some() {
        let mut out = args.to_vec();
        if let Some(v) = rewrite_trim {
            out[a.strategy_idx - 1] = b("=");
            out[a.strategy_idx] = v;
        }
        if auto_id {
            out[idpos] = id_str(id);
        }
        cx.propagate_as(out);
    }
    Ok(id_reply(id))
}

pub fn xtrim(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (a, _) = parse_add_trim(cx, args, false)?;
    let key = &args[1];
    if !cx.check_type(key, Kind::Stream)? {
        return Ok(Reply::Int(0));
    }
    let t = a.trim().expect("strategy checked");
    let mut km = cx.get_mut(key).expect("checked");
    let s = km.stream();
    let deleted = s.trim(&t);
    let rewrite = (deleted > 0 && a.approx).then(|| trim_rewrite(s, t.by));
    if deleted == 0 {
        km.unchanged();
    }
    drop(km);
    if deleted > 0 {
        cx.notify(notify::STREAM, "xtrim", key);
        if let Some(v) = rewrite {
            let mut out = args.to_vec();
            out[a.strategy_idx - 1] = b("=");
            out[a.strategy_idx] = v;
            cx.propagate_as(out);
        }
    }
    Ok(Reply::Int(deleted as i64))
}

// ---------------------------------------------------------------------------------------------
// XLEN / XRANGE / XREVRANGE
// ---------------------------------------------------------------------------------------------

pub fn xlen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.get_stream(&args[1])?.map_or(0, |s| s.len() as i64)))
}

fn xrange_generic(cx: &mut Cx, args: &[Bytes], rev: bool) -> CmdResult {
    let (start_arg, end_arg) = if rev { (&args[3], &args[2]) } else { (&args[2], &args[3]) };
    let start = range_start(start_arg)?;
    let end = range_end(end_arg)?;
    let mut count: i64 = -1;
    let mut j = 4;
    while j < args.len() {
        if eq(&args[j], "COUNT") && j + 1 < args.len() {
            count = int(&args[j + 1])?.max(0);
            j += 2;
        } else {
            return Err(SYNTAX);
        }
    }
    let Some(s) = cx.get_stream(&args[1])? else { return Ok(Reply::Array(vec![])) };
    if count == 0 {
        return Ok(Reply::NilArray);
    }
    let limit = if count < 0 { usize::MAX } else { count as usize };
    let items: Vec<Reply> = if rev {
        s.entries.rev_range(start, end).take(limit).map(|e| entry_reply(&e)).collect()
    } else {
        s.entries.range(start, end).take(limit).map(|e| entry_reply(&e)).collect()
    };
    Ok(Reply::Array(items))
}

pub fn xrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    xrange_generic(cx, args, false)
}

pub fn xrevrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    xrange_generic(cx, args, true)
}

// ---------------------------------------------------------------------------------------------
// XREAD / XREADGROUP
// ---------------------------------------------------------------------------------------------

/// Key positions of XREAD/XREADGROUP (`xreadGetKeys`).
pub fn xread_keys(args: &[Bytes]) -> Vec<usize> {
    let mut pos = None;
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if eq(a, "block") || eq(a, "count") || eq(a, "maxcount") || eq(a, "maxsize") || eq(a, "claim") {
            i += 1;
        } else if eq(a, "group") {
            i += 2;
        } else if eq(a, "noack") {
        } else if eq(a, "streams") {
            pos = Some(i);
            break;
        } else {
            break;
        }
        i += 1;
    }
    let Some(pos) = pos else { return vec![] };
    let n = args.len() - pos - 1;
    if n == 0 || !n.is_multiple_of(2) {
        return vec![];
    }
    ((pos + 1)..(pos + 1 + n / 2)).collect()
}

pub fn xread(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    xread_generic(cx, args, false)
}

pub fn xreadgroup(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let start = cx.eng.effects.len();
    let r = xread_generic(cx, args, true);
    cx.no_propagate();
    wrap(cx, start);
    r
}

/// The reply byte budget of XREAD/XREADGROUP MAXSIZE.
struct Budget {
    resp: u8,
    /// 0 = no limit.
    max: u64,
    /// Bytes emitted so far (deferred headers count once filled, like Redis).
    used: u64,
}

impl Budget {
    /// `streamReplyMaxsizeReached`: only once at least one entry was emitted.
    fn reached(&self, emitted: usize) -> bool {
        self.max > 0 && emitted > 0 && self.used >= self.max
    }
    fn add(&mut self, r: &Reply) {
        if self.max > 0 {
            self.used += rsize(r, self.resp);
        }
    }
    fn add_raw(&mut self, n: u64) {
        if self.max > 0 {
            self.used += n;
        }
    }
}

/// What a consumer-group read needs to know besides the stream.
struct GroupRead<'a> {
    key: &'a Bytes,
    group: &'a Bytes,
    consumer: &'a Bytes,
    now: i64,
    noack: bool,
}

/// `streamReplyWithRange` for a consumer group: claim idle PEL entries (`CLAIM`), serve the
/// consumer's history, or deliver new entries. Returns the entries; effects go to `props`.
#[allow(clippy::too_many_arguments)]
fn serve_group(
    entries: &Entries,
    g: &mut Group,
    r: &GroupRead,
    start: StreamId,
    count: usize,
    min_idle: i64,
    history: bool,
    claimed_only: bool,
    budget: &mut Budget,
    before: usize,
    props: &mut Vec<Vec<Bytes>>,
) -> Vec<Reply> {
    let now = r.now;
    let cname = &r.consumer[..];
    let group_last = id_str(g.last_id);
    let mut items: Vec<Reply> = Vec::new();

    if min_idle != -1 {
        let tail = g.order_tail();
        let mut processed = 0;
        let mut pos = None;
        while let Some((k, id)) = g.by_time_after(pos) {
            pos = Some(k);
            let n = g.nack(id).expect("listed");
            let idle = now.wrapping_sub(n.delivery_time) as u64;
            if idle < min_idle as u64 {
                break;
            }
            if budget.reached(before + items.len()) {
                break;
            }
            let delivery_count = n.delivery_count;
            if let Some(e) = entries.get(id) {
                let item = Reply::Array(vec![id_reply(id), fields_reply(&e), Reply::Int(idle as i64), Reply::Int(delivery_count as i64)]);
                budget.add(&item);
                items.push(item);
                g.assign(id, cname);
                g.bump_delivery_count(id);
                g.touch_nack(id, now);
                if let Some(c) = g.consumer_mut(cname) {
                    c.active_time = now;
                }
                let dc = g.nack(id).map_or(0, |n| n.delivery_count);
                props.push(xclaim_effect(r.key, r.group, cname, id_str(id), now, dc, &group_last));
                processed += 1;
                if count > 0 && processed >= count {
                    break;
                }
            }
            if Some(k) == tail {
                break;
            }
        }
    }

    if history {
        let limit = if count == 0 { usize::MAX } else { count };
        let ids: Vec<StreamId> = g.consumer(cname).map(|c| c.pel_from(start).take(limit).collect()).unwrap_or_default();
        for id in ids {
            if budget.reached(before + items.len()) {
                break;
            }
            let item = match entries.get(id) {
                Some(e) => {
                    g.bump_delivery_count(id);
                    g.touch_nack(id, now);
                    entry_reply(&e)
                }
                None => Reply::Array(vec![id_reply(id), Reply::NilArray]),
            };
            budget.add(&item);
            items.push(item);
        }
        return items;
    }

    if claimed_only || (count > 0 && count == items.len()) {
        return items;
    }

    let mut propagate_last = false;
    for e in entries.range(start, StreamId::MAX) {
        if budget.reached(before + items.len()) {
            break;
        }
        if e.id > g.last_id {
            if g.entries_read != INVALID_ENTRIES_READ && g.last_id >= entries.first_id() && !entries.range_has_tombstones(Some(g.last_id), None) {
                g.entries_read += 1;
            } else if entries.entries_added() > 0 {
                g.entries_read = entries.estimate_distance(e.id);
            }
            g.last_id = e.id;
            propagate_last = true;
        }
        let item = if min_idle != -1 {
            Reply::Array(vec![id_reply(e.id), fields_reply(&e), Reply::Int(0), Reply::Int(0)])
        } else {
            entry_reply(&e)
        };
        budget.add(&item);
        items.push(item);
        if !r.noack {
            if g.nack(e.id).is_some() {
                g.assign(e.id, cname);
                g.set_delivery_count(e.id, 1);
                g.touch_nack(e.id, now);
            } else {
                g.add_nack(e.id, Some(cname), now);
            }
            if let Some(c) = g.consumer_mut(cname) {
                c.active_time = now;
            }
            let dc = g.nack(e.id).map_or(1, |n| n.delivery_count);
            props.push(xclaim_effect(r.key, r.group, cname, id_str(e.id), now, dc, &group_last));
        }
        if count > 0 && items.len() == count {
            break;
        }
    }
    if propagate_last {
        props.push(setid_effect(r.key, r.group, g));
    }
    items
}

fn xread_generic(cx: &mut Cx, args: &[Bytes], xreadgroup: bool) -> CmdResult {
    let argc = args.len();
    let mut min_idle: i64 = -1;
    let mut timeout: i64 = -1;
    let mut count: i64 = 0;
    let mut maxcount: i64 = 0;
    let mut maxsize: i64 = 0;
    let mut streams_arg = 0;
    let mut nstreams = 0;
    let mut noack = false;
    let mut group: Option<(&Bytes, &Bytes)> = None;
    let now = cx.now;

    let mut i = 1;
    while i < argc {
        let more = argc - i - 1;
        let o = &args[i];
        if eq(o, "CLAIM") && more > 0 {
            if !xreadgroup {
                return Err(Error::new("ERR The CLAIM option is only supported by XREADGROUP. You called XREAD instead."));
            }
            i += 1;
            min_idle = parse_i64(&args[i]).ok_or(Error::new("ERR min-idle-time is not an integer or out of range"))?;
            if min_idle < 0 {
                return Err(Error::new("ERR min-idle-time must be a positive integer"));
            }
        } else if eq(o, "BLOCK") && more > 0 {
            i += 1;
            let t = parse_i64(&args[i]).ok_or(Error::new("ERR timeout is not an integer or out of range"))?;
            if t < 0 {
                return Err(Error::new("ERR timeout is negative"));
            }
            if t > 0 && t > i64::MAX - now {
                return Err(Error::new("ERR timeout is out of range"));
            }
            timeout = t;
        } else if eq(o, "COUNT") && more > 0 {
            i += 1;
            count = int(&args[i])?.max(0);
        } else if eq(o, "MAXCOUNT") && more > 0 {
            i += 1;
            maxcount = int(&args[i])?;
            if maxcount <= 0 {
                return Err(Error::new("ERR MAXCOUNT must be a positive integer"));
            }
        } else if eq(o, "MAXSIZE") && more > 0 {
            i += 1;
            maxsize = int(&args[i])?;
            if maxsize <= 0 {
                return Err(Error::new("ERR MAXSIZE must be a positive integer"));
            }
        } else if eq(o, "STREAMS") && more > 0 {
            streams_arg = i + 1;
            let n = argc - streams_arg;
            if !n.is_multiple_of(2) {
                let (name, symbol) = if xreadgroup { ("xreadgroup", "ID or '>'") } else { ("xread", "ID, '+', or '$'") };
                return Err(Error::msg(format!(
                    "ERR Unbalanced '{name}' list of streams: for each stream key an {symbol} must be specified."
                )));
            }
            nstreams = n / 2;
            break;
        } else if eq(o, "GROUP") && more >= 2 {
            if !xreadgroup {
                return Err(Error::new("ERR The GROUP option is only supported by XREADGROUP. You called XREAD instead."));
            }
            group = Some((&args[i + 1], &args[i + 2]));
            i += 2;
        } else if eq(o, "NOACK") {
            if !xreadgroup {
                return Err(Error::new("ERR The NOACK option is only supported by XREADGROUP. You called XREAD instead."));
            }
            noack = true;
        } else {
            return Err(SYNTAX);
        }
        i += 1;
    }
    if streams_arg == 0 {
        return Err(SYNTAX);
    }
    if xreadgroup && group.is_none() {
        return Err(Error::new("ERR Missing GROUP option for XREADGROUP"));
    }
    if maxcount > 0 && count > 0 && maxcount < count {
        return Err(Error::new("ERR MAXCOUNT must be greater than or equal to COUNT"));
    }
    let key_at = |j: usize| &args[streams_arg + j];
    let id_at = |j: usize| &args[streams_arg + nstreams + j];

    // Resolve the IDs (and check types / groups).
    let mut ids = vec![StreamId::MIN; nstreams];
    let unblocking = cx.mode == Mode::Unblock;
    for (j, slot) in ids.iter_mut().enumerate() {
        let key = key_at(j);
        // A blocked XREAD isn't woken by a key turning into another type: keep waiting.
        if unblocking && !xreadgroup && cx.check_type(key, Kind::Stream).is_err() {
            let keys = (0..nstreams).map(|j| Str::from_slice(key_at(j))).collect();
            return Ok(cx.block_on(keys, 0, Reply::NilArray));
        }
        let s = cx.get_stream(key)?;
        if let Some((g, _)) = group
            && s.is_none_or(|s| s.groups.get(g).is_none())
        {
            return Err(Error::msg(format!(
                "NOGROUP No such key '{}' or consumer group '{}' in XREADGROUP with GROUP option",
                show(key),
                show(g)
            )));
        }
        let ida = cstr(id_at(j));
        *slot = if ida == b"$" {
            if xreadgroup {
                return Err(Error::new("ERR The $ ID is meaningless in the context of XREADGROUP: you want to read the history of this consumer by specifying a proper ID, or use the > ID to get new messages. The $ ID would just return an empty result set."));
            }
            s.map_or(StreamId::MIN, |s| s.entries.last_id())
        } else if ida == b"+" {
            if xreadgroup {
                return Err(Error::new("ERR The + ID is meaningless in the context of XREADGROUP: you want to read the history of this consumer by specifying a proper ID, or use the > ID to get new messages. The + ID would just return an empty result set."));
            }
            match s {
                Some(s) if !s.is_empty() => decr_wrap(s.entries.last_valid_id().expect("non-empty")),
                _ => StreamId::MIN,
            }
        } else if ida == b">" {
            if !xreadgroup {
                return Err(Error::new("ERR The > ID can be specified only when calling XREADGROUP using the GROUP <group> <consumer> option."));
            }
            StreamId::MAX
        } else {
            parse_strict(id_at(j), 0)?
        };
    }

    // Serve synchronously what we can.
    let resp = cx.resp();
    let mut budget = Budget { resp, max: maxsize as u64, used: 0 };
    let mut out: Vec<(Bytes, Reply)> = Vec::new();
    let mut min_pel_time = i64::MAX;
    let mut total = 0usize;
    for (j, gt) in ids.iter_mut().enumerate() {
        if maxcount > 0 && total >= maxcount as usize {
            break;
        }
        if budget.reached(total) {
            break;
        }
        let key = key_at(j);
        if cx.get_stream(key)?.is_none() {
            continue;
        }
        let mut stream_count = count as usize;
        if maxcount > 0 {
            let remaining = maxcount as usize - total;
            if count == 0 || remaining < count as usize {
                stream_count = remaining;
            }
        }
        let Some((gname, cname)) = group else {
            let s = cx.get_stream(key)?.expect("exists");
            if s.is_empty() || s.entries.last_valid_id().expect("non-empty") <= *gt {
                continue;
            }
            budget.add_raw(if resp == 2 { 4 } else { 0 } + bulk_size(key));
            let limit = if stream_count == 0 { usize::MAX } else { stream_count };
            let mut items = Vec::new();
            for e in s.entries.range(incr_wrap(*gt), StreamId::MAX) {
                if items.len() >= limit || budget.reached(total + items.len()) {
                    break;
                }
                let item = entry_reply(&e);
                budget.add(&item);
                items.push(item);
            }
            budget.add_raw(hdr_size(items.len()));
            total += items.len();
            out.push((key.clone(), Reply::Array(items)));
            continue;
        };

        let r = GroupRead { key, group: gname, consumer: cname, now, noack };
        let mut props = Vec::new();
        let mut km = cx.get_mut(key).expect("exists");
        km.unchanged();
        let s = km.stream();
        let (entries, groups) = s.split_mut();
        let mut g = groups.get_mut(gname).expect("checked");
        let mut serve_claimed = false;
        if min_idle != -1 {
            let mut pos = None;
            while let Some((k, id)) = g.by_time_after(pos) {
                pos = Some(k);
                if !entries.exists(id) {
                    continue;
                }
                let t = g.nack(id).expect("listed").delivery_time;
                min_pel_time = min_pel_time.min(t);
                if now.wrapping_sub(t) as u64 >= min_idle as u64 {
                    serve_claimed = true;
                }
                break;
            }
        }
        let mut serve = false;
        let mut history = false;
        if *gt != StreamId::MAX {
            serve = true;
            history = true;
        } else if !entries.is_empty() {
            let maxid = entries.last_valid_id().expect("non-empty");
            if maxid > g.last_id {
                serve = true;
                *gt = g.last_id;
            }
        }
        let created = g.create_consumer(cname, now);
        if let Some(c) = g.consumer_mut(cname) {
            c.seen_time = now;
        }
        let mut claimed_only = false;
        if history {
            min_idle = -1;
        } else if !serve && serve_claimed {
            serve = true;
            claimed_only = true;
        }
        if serve {
            budget.add_raw(if resp == 2 { 4 } else { 0 } + bulk_size(key));
            let items =
                serve_group(entries, &mut g, &r, incr_wrap(*gt), stream_count, min_idle, history, claimed_only, &mut budget, total, &mut props);
            budget.add_raw(hdr_size(items.len()));
            total += items.len();
            out.push((key.clone(), Reply::Array(items)));
        }
        drop(g);
        drop(km);
        if created {
            dirty(cx);
            cx.notify(notify::STREAM, "xgroup-createconsumer", key);
        }
        let n_props = props.len();
        if n_props > 0 {
            dirty(cx);
        }
        for p in props {
            prop(cx, p);
        }
        if created && (noack || n_props == 0) {
            prop(cx, vec![b("XGROUP"), b("CREATECONSUMER"), key.clone(), gname.clone(), cname.clone()]);
        }
    }

    if !out.is_empty() {
        return Ok(if resp == 2 {
            Reply::Array(out.into_iter().map(|(k, v)| Reply::Array(vec![Reply::Bulk(k), v])).collect())
        } else {
            Reply::Map(out.into_iter().map(|(k, v)| (Reply::Bulk(k), v)).collect())
        });
    }

    if timeout != -1 {
        if !cx.can_block() {
            return Ok(Reply::NilArray);
        }
        // `$` becomes the concrete last ID so that a re-run doesn't keep moving the target.
        let mut rewritten: Option<Vec<Bytes>> = None;
        for (j, id) in ids.iter().enumerate() {
            if cstr(id_at(j)) == b"$" {
                rewritten.get_or_insert_with(|| args.to_vec())[streams_arg + nstreams + j] = id_str(*id);
            }
        }
        let wake_at = if min_idle != -1 {
            let base = if min_pel_time != i64::MAX { min_pel_time } else { now };
            min_idle.saturating_add(base).saturating_add(1)
        } else {
            0
        };
        let keys = (0..nstreams).map(|j| Str::from_slice(key_at(j))).collect();
        return Ok(cx.block_on_ex(keys, timeout, Reply::NilArray, rewritten, xreadgroup, wake_at));
    }
    Ok(Reply::NilArray)
}

// ---------------------------------------------------------------------------------------------
// XDEL / XDELEX / XACKDEL
// ---------------------------------------------------------------------------------------------

/// Bookkeeping after deleting entries (`max-deleted-entry-id`, recorded first ID).
fn after_delete(e: &mut Entries, first_deleted: bool) {
    if e.is_empty() {
        e.set_first_id(StreamId::MIN);
    } else if first_deleted {
        e.refresh_first_id();
    }
}

/// Delete one entry; returns whether it existed. Tracks first-entry deletion.
fn delete_one(e: &mut Entries, id: StreamId, first_deleted: &mut bool) -> bool {
    if !e.delete(id) {
        return false;
    }
    if id == e.first_id() {
        *first_deleted = true;
    }
    e.note_deleted(id);
    true
}

pub fn xdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    if !cx.check_type(key, Kind::Stream)? {
        return Ok(Reply::Int(0));
    }
    let ids = args[2..].iter().map(|a| parse_strict(a, 0)).collect::<Result<Vec<_>, _>>()?;
    let mut km = cx.get_mut(key).expect("checked");
    let e = &mut km.stream().entries;
    let mut deleted = 0;
    let mut first = false;
    for id in ids {
        if delete_one(e, id, &mut first) {
            deleted += 1;
        }
    }
    if deleted > 0 {
        after_delete(e, first);
    } else {
        km.unchanged();
    }
    drop(km);
    if deleted > 0 {
        cx.notify(notify::STREAM, "xdel", key);
    }
    Ok(Reply::Int(deleted))
}

/// `streamParseAckDelArgsOrReply`: returns (policy, first id index, numids).
fn parse_ackdel(args: &[Bytes], start: usize) -> Result<(RefPolicy, usize, usize), Error> {
    let argc = args.len();
    let mut policy = None;
    let mut ids = None;
    let mut j = start;
    while j < argc {
        let o = &args[j];
        if eq(o, "KEEPREF") && policy.is_none() {
            policy = Some(RefPolicy::KeepRef);
            j += 1;
        } else if eq(o, "DELREF") && policy.is_none() {
            policy = Some(RefPolicy::DelRef);
            j += 1;
        } else if eq(o, "ACKED") && policy.is_none() {
            policy = Some(RefPolicy::Acked);
            j += 1;
        } else if eq(o, "IDS") && j + 1 < argc {
            let n = parse_i64(&args[j + 1])
                .filter(|&n| n >= 1)
                .ok_or(Error::new("ERR Number of IDs must be a positive integer"))? as usize;
            if n > argc - j - 2 {
                return Err(Error::new("ERR The `numids` parameter must match the number of arguments"));
            }
            ids = Some((j + 2, n));
            j = j + 2 + n;
        } else {
            return Err(SYNTAX);
        }
    }
    let Some((first, n)) = ids else { return Err(Error::new("ERR IDS option is required")) };
    Ok((policy.unwrap_or_default(), first, n))
}

fn not_found_array(n: usize) -> Reply {
    Reply::Array(vec![Reply::Int(-1); n])
}

pub fn xdelex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let exists = cx.check_type(key, Kind::Stream)?;
    let (policy, first, n) = parse_ackdel(args, 2)?;
    if !exists {
        return Ok(not_found_array(n));
    }
    let ids = args[first..first + n].iter().map(|a| parse_strict(a, 0)).collect::<Result<Vec<_>, _>>()?;
    let mut km = cx.get_mut(key).expect("checked");
    let (e, groups) = km.stream().split_mut();
    let mut out = Vec::with_capacity(n);
    let mut deleted = 0;
    let mut first_deleted = false;
    for id in ids {
        let can = match policy {
            RefPolicy::Acked => !groups.is_referenced(id),
            RefPolicy::DelRef => {
                groups.cleanup_refs(id);
                true
            }
            RefPolicy::KeepRef => true,
        };
        out.push(Reply::Int(if !can {
            2
        } else if delete_one(e, id, &mut first_deleted) {
            deleted += 1;
            1
        } else {
            -1
        }));
    }
    if deleted > 0 {
        after_delete(e, first_deleted);
    } else {
        km.unchanged();
    }
    drop(km);
    if deleted > 0 {
        cx.notify(notify::STREAM, "xdel", key);
    }
    Ok(Reply::Array(out))
}

pub fn xackdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let gname = &args[2];
    let has_group = cx.get_stream(key)?.map(|s| s.groups.get(gname).is_some());
    let (policy, first, n) = parse_ackdel(args, 3)?;
    if has_group != Some(true) {
        return Ok(not_found_array(n));
    }
    let ids = args[first..first + n].iter().map(|a| parse_strict(a, 0)).collect::<Result<Vec<_>, _>>()?;
    let mut km = cx.get_mut(key).expect("checked");
    let (e, groups) = km.stream().split_mut();
    let mut out = Vec::with_capacity(n);
    let mut deleted = 0;
    let mut acked = 0;
    let mut first_deleted = false;
    for id in ids {
        let mut res = -1;
        let removed = groups.get_mut(gname).expect("checked").remove_nack(id).is_some();
        if removed {
            acked += 1;
            let can = match policy {
                RefPolicy::Acked => !groups.is_referenced(id),
                RefPolicy::DelRef => {
                    groups.cleanup_refs(id);
                    true
                }
                RefPolicy::KeepRef => true,
            };
            if can && delete_one(e, id, &mut first_deleted) {
                deleted += 1;
            }
            res = if can { 1 } else { 2 };
        }
        out.push(Reply::Int(res));
    }
    if deleted > 0 {
        after_delete(e, first_deleted);
    } else {
        km.unchanged();
    }
    drop(km);
    if deleted > 0 {
        cx.notify(notify::STREAM, "xdel", key);
    } else if acked > 0 {
        dirty(cx);
    }
    Ok(Reply::Array(out))
}

// ---------------------------------------------------------------------------------------------
// XGROUP
// ---------------------------------------------------------------------------------------------

/// `addReplySubcommandSyntaxError`.
fn sub_syntax(args: &[Bytes]) -> Error {
    let sub: String = show(&args[1]).chars().take(128).collect();
    Error::msg(format!(
        "ERR unknown subcommand or wrong number of arguments for '{sub}'. Try {} HELP.",
        show(&args[0]).to_uppercase()
    ))
}

/// The shared prologue of XGROUP subcommands: options (CREATE/SETID), key and group checks.
/// Returns (mkstream, entries_read, key exists).
fn xgroup_prologue(cx: &mut Cx, args: &[Bytes], need_group: bool) -> Result<(bool, i64, bool), Error> {
    let create = eq(&args[1], "CREATE");
    let setid = eq(&args[1], "SETID");
    let mut mkstream = false;
    let mut entries_read = INVALID_ENTRIES_READ;
    let mut i = 5;
    while i < args.len() {
        if create && eq(&args[i], "MKSTREAM") {
            mkstream = true;
            i += 1;
        } else if (create || setid) && eq(&args[i], "ENTRIESREAD") && i + 1 < args.len() {
            entries_read = int(&args[i + 1])?;
            if entries_read < 0 && entries_read != INVALID_ENTRIES_READ {
                return Err(Error::new("ERR value for ENTRIESREAD must be positive or -1"));
            }
            i += 2;
        } else {
            return Err(sub_syntax(args));
        }
    }
    let exists = cx.check_type(&args[2], Kind::Stream)?;
    if !mkstream {
        if !exists {
            return Err(Error::new("ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may want to use the MKSTREAM option to create an empty stream automatically."));
        }
        if need_group {
            let found = cx.peek(&args[2]).is_some_and(|e| match &e.value {
                crate::value::Value::Stream(s) => s.groups.get(&args[3]).is_some(),
                _ => false,
            });
            if !found {
                return Err(Error::msg(format!(
                    "NOGROUP No such consumer group '{}' for key name '{}'",
                    show(&args[3]),
                    show(&args[2])
                )));
            }
        }
    }
    Ok((mkstream, entries_read, exists))
}

pub fn xgroup_create(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (_, mut entries_read, exists) = xgroup_prologue(cx, args, false)?;
    if args.len() > 8 {
        return Err(sub_syntax(args));
    }
    let key = &args[2];
    let id = if cstr(&args[4]) == b"$" {
        if exists {
            cx.get_mut(key).map(|mut k| {
                k.unchanged();
                k.stream().entries.last_id()
            })
        } else {
            None
        }
        .unwrap_or(StreamId::MIN)
    } else {
        parse_strict(&args[4], 0)?
    };
    let (dur, max) = (cx.cfg().stream_idmp_duration, cx.cfg().stream_idmp_maxsize);
    let mut km = if exists {
        let mut k = cx.get_mut(key).expect("exists");
        k.unchanged();
        k
    } else {
        // MKSTREAM: created (and signalled) like any new key.
        let mut k = cx.get_or_create(key, Kind::Stream)?;
        let s = k.stream();
        s.idmp_duration = dur;
        s.idmp_maxsize = max;
        k
    };
    let s = km.stream();
    if entries_read != INVALID_ENTRIES_READ && entries_read as u64 > s.entries.entries_added() {
        entries_read = s.entries.entries_added() as i64;
    }
    let created = s.groups.create(&args[3], id, entries_read);
    drop(km);
    if !created {
        return Err(Error::new("BUSYGROUP Consumer Group name already exists"));
    }
    dirty(cx);
    cx.notify(notify::STREAM, "xgroup-create", key);
    Ok(Reply::Ok)
}

pub fn xgroup_setid(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (_, mut entries_read, _) = xgroup_prologue(cx, args, true)?;
    if args.len() != 5 && args.len() != 7 {
        return Err(sub_syntax(args));
    }
    let key = &args[2];
    let given = if cstr(&args[4]) == b"$" { None } else { Some(parse_loose(&args[4], 0)?) };
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let s = km.stream();
    let id = given.unwrap_or(s.entries.last_id());
    if entries_read != INVALID_ENTRIES_READ && entries_read as u64 > s.entries.entries_added() {
        entries_read = s.entries.entries_added() as i64;
    }
    let mut g = s.groups.get_mut(&args[3]).expect("checked");
    g.last_id = id;
    g.entries_read = entries_read;
    drop(g);
    drop(km);
    dirty(cx);
    cx.notify(notify::STREAM, "xgroup-setid", key);
    Ok(Reply::Ok)
}

pub fn xgroup_destroy(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    xgroup_prologue(cx, args, false)?;
    let key = &args[2];
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let removed = km.stream().groups.remove(&args[3]).is_some();
    drop(km);
    if !removed {
        return Ok(Reply::Int(0));
    }
    dirty(cx);
    cx.notify(notify::STREAM, "xgroup-destroy", key);
    cx.signal_ready(key);
    Ok(Reply::Int(1))
}

pub fn xgroup_createconsumer(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    xgroup_prologue(cx, args, true)?;
    let key = &args[2];
    let now = cx.now;
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let created = km.stream().groups.get_mut(&args[3]).expect("checked").create_consumer(&args[4], now);
    drop(km);
    if created {
        dirty(cx);
        cx.notify(notify::STREAM, "xgroup-createconsumer", key);
    }
    Ok(Reply::Int(created as i64))
}

pub fn xgroup_delconsumer(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    xgroup_prologue(cx, args, true)?;
    let key = &args[2];
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let pending = km.stream().groups.get_mut(&args[3]).expect("checked").delete_consumer(&args[4]);
    drop(km);
    if pending.is_some() {
        dirty(cx);
        cx.notify(notify::STREAM, "xgroup-delconsumer", key);
    }
    Ok(Reply::Int(pending.unwrap_or(0) as i64))
}

pub fn xgroup_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "XGROUP <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "CREATE <key> <groupname> <id|$> [option]",
        "    Create a new consumer group. Options are:",
        "    * MKSTREAM",
        "      Create the empty stream if it does not exist.",
        "    * ENTRIESREAD entries_read",
        "      Set the group's entries_read counter (internal use).",
        "CREATECONSUMER <key> <groupname> <consumer>",
        "    Create a new consumer in the specified group.",
        "DELCONSUMER <key> <groupname> <consumer>",
        "    Remove the specified consumer.",
        "DESTROY <key> <groupname>",
        "    Remove the specified group.",
        "SETID <key> <groupname> <id|$> [ENTRIESREAD entries_read]",
        "    Set the current group ID and entries_read counter.",
        "HELP",
        "    Print this help.",
    ]))
}

// ---------------------------------------------------------------------------------------------
// XSETID / XCFGSET / XIDMPRECORD
// ---------------------------------------------------------------------------------------------

pub fn xsetid(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let id = parse_strict(&args[2], 0)?;
    let mut entries_added: i64 = -1;
    let mut max_xdel = StreamId::MIN;
    let mut i = 3;
    while i < args.len() {
        let more = args.len() - 1 - i;
        if eq(&args[i], "ENTRIESADDED") && more > 0 {
            entries_added = int(&args[i + 1])?;
            if entries_added < 0 {
                return Err(Error::new("ERR entries_added must be positive"));
            }
            i += 2;
        } else if eq(&args[i], "MAXDELETEDID") && more > 0 {
            max_xdel = parse_strict(&args[i + 1], 0)?;
            if id < max_xdel {
                return Err(Error::new("ERR The ID specified in XSETID is smaller than the provided max_deleted_entry_id"));
            }
            i += 2;
        } else {
            return Err(SYNTAX);
        }
    }
    let key = &args[1];
    if !cx.check_type(key, Kind::Stream)? {
        return Err(NO_KEY);
    }
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let e = &mut km.stream().entries;
    if id < e.max_deleted_id() {
        return Err(Error::new("ERR The ID specified in XSETID is smaller than current max_deleted_entry_id"));
    }
    if !e.is_empty() {
        if id < e.last_valid_id().expect("non-empty") {
            return Err(Error::new("ERR The ID specified in XSETID is smaller than the target stream top item"));
        }
        if entries_added != -1 && e.len() > entries_added as u64 {
            return Err(Error::new("ERR The entries_added specified in XSETID is smaller than the target stream length"));
        }
    }
    e.set_last_id(id);
    if entries_added != -1 {
        e.set_entries_added(entries_added as u64);
    }
    if !max_xdel.is_zero() {
        e.set_max_deleted_id(max_xdel);
    }
    drop(km);
    dirty(cx);
    cx.notify(notify::STREAM, "xsetid", key);
    Ok(Reply::Ok)
}

pub fn xcfgset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    if !cx.check_type(key, Kind::Stream)? {
        return Err(NO_KEY);
    }
    let mut duration: i64 = -1;
    let mut maxsize: i64 = -1;
    let mut i = 2;
    while i < args.len() {
        let more = args.len() - i - 1;
        if eq(&args[i], "IDMP-DURATION") && more > 0 {
            if duration != -1 {
                return Err(Error::new("ERR IDMP-DURATION specified multiple times"));
            }
            i += 1;
            duration = int(&args[i])?;
            if !(1..=86400).contains(&duration) {
                return Err(Error::new("ERR IDMP-DURATION must be between 1 and 86400 seconds"));
            }
        } else if eq(&args[i], "IDMP-MAXSIZE") && more > 0 {
            if maxsize != -1 {
                return Err(Error::new("ERR IDMP-MAXSIZE specified multiple times"));
            }
            i += 1;
            maxsize = int(&args[i])?;
            if !(1..=10000).contains(&maxsize) {
                return Err(Error::new("ERR IDMP-MAXSIZE must be between 1 and 10000 entries"));
            }
        } else {
            return Err(SYNTAX);
        }
        i += 1;
    }
    if duration == -1 && maxsize == -1 {
        return Err(Error::new("ERR At least one parameter must be specified"));
    }
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let s = km.stream();
    let mut changed = false;
    if duration != -1 && s.idmp_duration != duration as u64 {
        s.idmp_duration = duration as u64;
        s.idmp.clear();
        changed = true;
    }
    if maxsize != -1 && s.idmp_maxsize != maxsize as u64 {
        s.idmp_maxsize = maxsize as u64;
        s.idmp.clear();
        changed = true;
    }
    drop(km);
    if changed {
        dirty(cx);
    }
    Ok(Reply::Ok)
}

pub fn xidmprecord(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let id = parse_strict(&args[4], 0)?;
    let (pid, iid) = (&args[2], &args[3]);
    if pid.is_empty() {
        return Err(Error::new("ERR producer ID must be non-empty"));
    }
    if iid.is_empty() {
        return Err(Error::new("ERR idempotent ID must be non-empty"));
    }
    let key = &args[1];
    if !cx.check_type(key, Kind::Stream)? {
        return Err(NO_KEY);
    }
    let now = cx.now;
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let s = km.stream();
    if !s.entries.exists(id) {
        return Err(Error::new("ERR No such message in stream"));
    }
    s.idmp.expire(idmp_cutoff(now, s));
    s.idmp.ensure_producer(pid);
    match s.idmp.lookup(pid, iid) {
        Some(found) if found == id => return Ok(Reply::Ok),
        Some(_) => return Err(Error::new("ERR IID already exists for this producer with a different stream ID")),
        None => {}
    }
    let max = s.idmp_maxsize;
    s.idmp.insert(pid, iid, id, max);
    s.iids_added += 1;
    drop(km);
    dirty(cx);
    Ok(Reply::Ok)
}

// ---------------------------------------------------------------------------------------------
// XACK / XNACK / XPENDING
// ---------------------------------------------------------------------------------------------

pub fn xack(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let gname = &args[2];
    let has_group = cx.get_stream(key)?.is_some_and(|s| s.groups.get(gname).is_some());
    if !has_group {
        return Ok(Reply::Int(0));
    }
    let ids = args[3..].iter().map(|a| parse_strict(a, 0)).collect::<Result<Vec<_>, _>>()?;
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let mut g = km.stream().groups.get_mut(gname).expect("checked");
    let acked = ids.into_iter().filter(|&id| g.remove_nack(id).is_some()).count();
    drop(g);
    drop(km);
    if acked > 0 {
        dirty(cx);
    }
    Ok(Reply::Int(acked as i64))
}

pub fn xnack(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let gname = &args[2];
    let exists = cx.check_type(key, Kind::Stream)?;
    let has_group = exists
        && cx.peek(key).is_some_and(|e| match &e.value {
            crate::value::Value::Stream(s) => s.groups.get(gname).is_some(),
            _ => false,
        });
    if !has_group {
        return Err(nogroup(key, gname));
    }
    #[derive(PartialEq)]
    enum M {
        Silent,
        Fail,
        Fatal,
    }
    let mode = if eq(&args[3], "SILENT") {
        M::Silent
    } else if eq(&args[3], "FAIL") {
        M::Fail
    } else if eq(&args[3], "FATAL") {
        M::Fatal
    } else {
        return Err(Error::new("ERR mode must be SILENT, FAIL, or FATAL"));
    };
    let argc = args.len();
    let mut ids_start = 0;
    let mut numids = 0;
    let mut force = false;
    let mut retrycount: i64 = -1;
    let mut i = 4;
    while i < argc {
        let more = argc - 1 - i;
        let o = &args[i];
        if eq(o, "IDS") && more > 0 {
            let n = parse_i64(&args[i + 1])
                .filter(|&n| (1..=i32::MAX as i64).contains(&n))
                .ok_or(Error::new("ERR numids must be a positive integer"))? as usize;
            ids_start = i + 2;
            if n > argc - ids_start {
                return Err(Error::new("ERR number of IDs doesn't match numids"));
            }
            numids = n;
            i = ids_start + numids - 1;
        } else if eq(o, "FORCE") {
            force = true;
        } else if eq(o, "RETRYCOUNT") && more > 0 {
            i += 1;
            retrycount = int(&args[i])?;
            if retrycount < 0 {
                return Err(Error::new("ERR Invalid RETRYCOUNT value, must be >= 0"));
            }
        } else {
            return Err(Error::msg(format!("ERR Unrecognized XNACK option '{}'", show(o))));
        }
        i += 1;
    }
    if ids_start == 0 {
        return Err(Error::new("ERR syntax error, expected IDS keyword"));
    }
    let ids = args[ids_start..ids_start + numids].iter().map(|a| parse_strict(a, 0)).collect::<Result<Vec<_>, _>>()?;
    let adjust = |count: u64| -> u64 {
        if retrycount >= 0 {
            retrycount as u64
        } else {
            match mode {
                M::Silent => count.saturating_sub(1),
                M::Fail => count,
                M::Fatal => i64::MAX as u64,
            }
        }
    };
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let (entries, groups) = km.stream().split_mut();
    let mut g = groups.get_mut(gname).expect("checked");
    let mut nacked = 0;
    for id in ids {
        if let Some(n) = g.nack(id) {
            let c = adjust(n.delivery_count);
            g.set_delivery_count(id, c);
            g.release(id);
        } else if force {
            if !entries.exists(id) {
                continue;
            }
            g.add_nacked(id, adjust(0));
        } else {
            continue;
        }
        nacked += 1;
    }
    drop(g);
    drop(km);
    if nacked > 0 {
        dirty(cx);
        cx.signal_ready(key);
    }
    Ok(Reply::Int(nacked))
}

pub fn xpending(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let argc = args.len();
    let justinfo = argc == 3;
    if argc != 3 && !(6..=9).contains(&argc) {
        return Err(SYNTAX);
    }
    let mut minidle: i64 = 0;
    let mut count: i64 = 0;
    let mut start = StreamId::MIN;
    let mut end = StreamId::MAX;
    let mut consumer: Option<&Bytes> = None;
    if argc >= 6 {
        let mut si = 3;
        if eq(&args[3], "IDLE") {
            minidle = int(&args[4])?;
            if argc < 8 {
                return Err(SYNTAX);
            }
            si += 2;
        }
        count = int(&args[si + 2])?.max(0);
        start = range_start(&args[si])?;
        end = range_end(&args[si + 1])?;
        if si + 3 < argc {
            consumer = Some(&args[si + 3]);
        }
    }
    let key = &args[1];
    let gname = &args[2];
    let now = cx.now;
    let Some(s) = cx.get_stream(key)? else { return Err(nogroup(key, gname)) };
    let Some(g) = s.groups.get(gname) else { return Err(nogroup(key, gname)) };
    if justinfo {
        if g.pel_len() == 0 {
            return Ok(Reply::Array(vec![Reply::Int(0), Reply::Nil, Reply::Nil, Reply::NilArray]));
        }
        let first = g.pel().next().expect("non-empty").0;
        let last = g.pel().next_back().expect("non-empty").0;
        let consumers = g
            .consumers()
            .filter(|(_, c)| c.pending() > 0)
            .map(|(name, c)| Reply::Array(vec![Reply::bulk(name), Reply::Bulk(num(c.pending() as i64))]))
            .collect();
        return Ok(Reply::Array(vec![Reply::Int(g.pel_len() as i64), id_reply(first), id_reply(last), Reply::Array(consumers)]));
    }
    let ids: Vec<StreamId> = match consumer {
        Some(c) => match g.consumer(c) {
            None => return Ok(Reply::Array(vec![])),
            Some(c) => c.pel_from(start).take_while(|&id| id <= end).collect(),
        },
        None => g.pel_range(start, end).map(|(id, _)| id).collect(),
    };
    let mut out = Vec::new();
    for id in ids {
        if count == 0 {
            break;
        }
        let Some(n) = g.nack(id) else { continue };
        if n.consumer.is_some() && minidle != 0 && now - n.delivery_time < minidle {
            continue;
        }
        count -= 1;
        let (name, elapsed) = match &n.consumer {
            Some(c) => (Reply::bulk(c), (now - n.delivery_time).max(0)),
            None => (Reply::bulk(""), -1),
        };
        out.push(Reply::Array(vec![id_reply(id), name, Reply::Int(elapsed), Reply::Int(n.delivery_count as i64)]));
    }
    Ok(Reply::Array(out))
}

// ---------------------------------------------------------------------------------------------
// XCLAIM / XAUTOCLAIM
// ---------------------------------------------------------------------------------------------

pub fn xclaim(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let start = cx.eng.effects.len();
    let r = xclaim_inner(cx, args);
    wrap(cx, start);
    r
}

fn xclaim_inner(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (key, gname, cname) = (&args[1], &args[2], &args[3]);
    let has_group = cx.get_stream(key)?.is_some_and(|s| s.groups.get(gname).is_some());
    if !has_group {
        return Err(nogroup(key, gname));
    }
    let minidle = parse_i64(&args[4]).ok_or(Error::new("ERR Invalid min-idle-time argument for XCLAIM"))?.max(0);
    let argc = args.len();
    let mut ids = Vec::new();
    let mut j = 5;
    while j < argc {
        match parse_strict(&args[j], 0) {
            Ok(id) => ids.push((id, j)),
            Err(_) => break,
        }
        j += 1;
    }
    let now = cx.now;
    let (mut force, mut justid) = (false, false);
    let mut deliverytime: i64 = -1;
    let mut retrycount: i64 = -1;
    let mut last_id = StreamId::MIN;
    while j < argc {
        let more = argc - 1 - j;
        let opt = &args[j];
        if eq(opt, "FORCE") {
            force = true;
        } else if eq(opt, "JUSTID") {
            justid = true;
        } else if eq(opt, "IDLE") && more > 0 {
            j += 1;
            let v = parse_i64(&args[j]).ok_or(Error::new("ERR Invalid IDLE option argument for XCLAIM"))?;
            deliverytime = now.wrapping_sub(v);
        } else if eq(opt, "TIME") && more > 0 {
            j += 1;
            deliverytime = parse_i64(&args[j]).ok_or(Error::new("ERR Invalid TIME option argument for XCLAIM"))?;
        } else if eq(opt, "RETRYCOUNT") && more > 0 {
            j += 1;
            retrycount = parse_i64(&args[j]).ok_or(Error::new("ERR Invalid RETRYCOUNT option argument for XCLAIM"))?;
        } else if eq(opt, "LASTID") && more > 0 {
            j += 1;
            last_id = parse_strict(&args[j], 0)?;
        } else {
            return Err(Error::msg(format!("ERR Unrecognized XCLAIM option '{}'", show(opt))));
        }
        j += 1;
    }
    if deliverytime == -1 || deliverytime < 0 || deliverytime > now {
        deliverytime = now;
    }

    let mut props: Vec<Vec<Bytes>> = Vec::new();
    let mut items = Vec::new();
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let (entries, groups) = km.stream().split_mut();
    let mut g = groups.get_mut(gname).expect("checked");
    let mut propagate_last = false;
    if last_id > g.last_id {
        g.last_id = last_id;
        propagate_last = true;
    }
    let created = g.create_consumer(cname, now);
    if let Some(c) = g.consumer_mut(cname) {
        c.seen_time = now;
    }
    for (id, j) in ids {
        if !entries.exists(id) {
            if let Some(n) = g.nack(id) {
                match &n.consumer {
                    Some(c) => {
                        props.push(xclaim_effect(key, gname, c, args[j].clone(), n.delivery_time, n.delivery_count, &id_str(g.last_id)));
                        propagate_last = false;
                    }
                    None => props.push(vec![b("XACK"), key.clone(), gname.clone(), args[j].clone()]),
                }
                g.remove_nack(id);
            }
            continue;
        }
        if force && g.nack(id).is_none() {
            g.add_nack(id, None, now);
        }
        let Some(n) = g.nack(id) else { continue };
        if n.consumer.is_some() && minidle != 0 && now - n.delivery_time < minidle {
            continue;
        }
        g.touch_nack(id, deliverytime);
        if retrycount >= 0 {
            g.set_delivery_count(id, retrycount as u64);
        } else if !justid {
            g.bump_delivery_count(id);
        }
        g.assign(id, cname);
        items.push(if justid { id_reply(id) } else { entry_reply(&entries.get(id).expect("exists")) });
        if let Some(c) = g.consumer_mut(cname) {
            c.active_time = now;
        }
        let n = g.nack(id).expect("present");
        props.push(xclaim_effect(key, gname, cname, args[j].clone(), n.delivery_time, n.delivery_count, &id_str(g.last_id)));
        propagate_last = false;
    }
    if propagate_last {
        props.push(setid_effect(key, gname, &g));
    }
    drop(g);
    drop(km);
    if created {
        dirty(cx);
        cx.notify(notify::STREAM, "xgroup-createconsumer", key);
    }
    if !props.is_empty() {
        dirty(cx);
    }
    for p in props {
        prop(cx, p);
    }
    cx.no_propagate();
    Ok(Reply::Array(items))
}

pub fn xautoclaim(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let start = cx.eng.effects.len();
    let r = xautoclaim_inner(cx, args);
    wrap(cx, start);
    r
}

fn xautoclaim_inner(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (key, gname, cname) = (&args[1], &args[2], &args[3]);
    let minidle = parse_i64(&args[4]).ok_or(Error::new("ERR Invalid min-idle-time argument for XAUTOCLAIM"))?.max(0);
    let start = range_start(&args[5])?;
    let mut count: i64 = 100;
    let mut justid = false;
    let mut j = 6;
    while j < args.len() {
        let more = args.len() - 1 - j;
        if eq(&args[j], "COUNT") && more > 0 {
            count = parse_i64(&args[j + 1])
                .filter(|&n| (1..=i64::MAX / 16).contains(&n))
                .ok_or(Error::new("ERR COUNT must be > 0"))?;
            j += 1;
        } else if eq(&args[j], "JUSTID") {
            justid = true;
        } else {
            return Err(SYNTAX);
        }
        j += 1;
    }
    let has_group = cx.get_stream(key)?.is_some_and(|s| s.groups.get(gname).is_some());
    if !has_group {
        return Err(nogroup(key, gname));
    }
    let now = cx.now;
    let mut props: Vec<Vec<Bytes>> = Vec::new();
    let mut claimed = Vec::new();
    let mut deleted_ids = Vec::new();
    let mut km = cx.get_mut(key).expect("checked");
    km.unchanged();
    let (entries, groups) = km.stream().split_mut();
    let mut g = groups.get_mut(gname).expect("checked");
    let created = g.create_consumer(cname, now);
    if let Some(c) = g.consumer_mut(cname) {
        c.seen_time = now;
    }
    let mut attempts = count.saturating_mul(10);
    // Lower bound (inclusive) of the next PEL entry to look at; None past the maximal ID.
    let mut from = Some(start);
    let mut eof = false;
    loop {
        if attempts == 0 {
            break;
        }
        attempts -= 1;
        if count == 0 {
            break;
        }
        let Some(id) = from.and_then(|f| g.pel_next(f)) else {
            eof = true;
            break;
        };
        from = id.next();
        let Some(e) = entries.get(id) else {
            let n = g.nack(id).expect("listed");
            match &n.consumer {
                Some(c) => props.push(xclaim_effect(key, gname, c, id_str(id), n.delivery_time, n.delivery_count, &id_str(g.last_id))),
                None => props.push(vec![b("XACK"), key.clone(), gname.clone(), id_str(id)]),
            }
            g.remove_nack(id);
            deleted_ids.push(id_reply(id));
            count -= 1;
            continue;
        };
        let n = g.nack(id).expect("listed");
        if n.consumer.is_some() && minidle != 0 && now - n.delivery_time < minidle {
            continue;
        }
        g.touch_nack(id, now);
        if !justid {
            g.bump_delivery_count(id);
        }
        g.assign(id, cname);
        claimed.push(if justid { id_reply(id) } else { entry_reply(&e) });
        count -= 1;
        if let Some(c) = g.consumer_mut(cname) {
            c.active_time = now;
        }
        let n = g.nack(id).expect("present");
        props.push(xclaim_effect(key, gname, cname, id_str(id), n.delivery_time, n.delivery_count, &id_str(g.last_id)));
    }
    let cursor = if eof { StreamId::MIN } else { from.and_then(|f| g.pel_next(f)).unwrap_or(StreamId::MIN) };
    drop(g);
    drop(km);
    if created {
        dirty(cx);
        cx.notify(notify::STREAM, "xgroup-createconsumer", key);
    }
    if !props.is_empty() {
        dirty(cx);
    }
    for p in props {
        prop(cx, p);
    }
    cx.no_propagate();
    Ok(Reply::Array(vec![id_reply(cursor), Reply::Array(claimed), Reply::Array(deleted_ids)]))
}

// ---------------------------------------------------------------------------------------------
// XINFO
// ---------------------------------------------------------------------------------------------

fn kv(k: &'static str, v: Reply) -> (Reply, Reply) {
    (Reply::bulk(k), v)
}

fn lag_reply(e: &Entries, g: &Group) -> Reply {
    match e.lag(g) {
        Some(l) => Reply::Int(l),
        None => Reply::Nil,
    }
}

fn entries_read_reply(g: &Group) -> Reply {
    if g.entries_read != INVALID_ENTRIES_READ { Reply::Int(g.entries_read) } else { Reply::Nil }
}

pub fn xinfo_stream(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let now = cx.now;
    let Some(s) = cx.get_stream(&args[2])? else { return Err(NO_KEY) };
    let optv = &args[3..];
    let mut full = true;
    let mut count: i64 = 10;
    if optv.is_empty() {
        full = false;
    } else {
        if optv.len() != 1 && optv.len() != 3 {
            return Err(sub_syntax(args));
        }
        if !eq(&optv[0], "full") {
            return Err(sub_syntax(args));
        }
        if optv.len() == 3 {
            if !eq(&optv[1], "count") {
                return Err(sub_syntax(args));
            }
            count = int(&optv[2])?;
            if count < 0 {
                count = 10;
            }
        }
    }
    let e = &s.entries;
    let (pids, iids) = s.idmp.live_counts(idmp_cutoff(now, s));
    let mut m = vec![
        kv("length", Reply::Int(e.len() as i64)),
        kv("radix-tree-keys", Reply::Int(e.node_count() as i64)),
        kv("radix-tree-nodes", Reply::Int(e.rax_nodes() as i64)),
        kv("last-generated-id", id_reply(e.last_id())),
        kv("max-deleted-entry-id", id_reply(e.max_deleted_id())),
        kv("entries-added", Reply::Int(e.entries_added() as i64)),
        kv("recorded-first-entry-id", id_reply(e.first_id())),
        kv("idmp-duration", Reply::Int(s.idmp_duration as i64)),
        kv("idmp-maxsize", Reply::Int(s.idmp_maxsize as i64)),
        kv("pids-tracked", Reply::Int(pids as i64)),
        kv("iids-tracked", Reply::Int(iids as i64)),
        kv("iids-added", Reply::Int(s.iids_added as i64)),
        kv("iids-duplicates", Reply::Int(s.iids_duplicates as i64)),
    ];
    if !full {
        m.push(kv("groups", Reply::Int(s.groups.len() as i64)));
        let first = e.range(StreamId::MIN, StreamId::MAX).next();
        m.push(kv("first-entry", first.map_or(Reply::Nil, |x| entry_reply(&x))));
        let last = e.rev_range(StreamId::MIN, StreamId::MAX).next();
        m.push(kv("last-entry", last.map_or(Reply::Nil, |x| entry_reply(&x))));
        return Ok(Reply::Map(m));
    }
    let limit = if count == 0 { usize::MAX } else { count as usize };
    m.push(kv("entries", Reply::Array(e.range(StreamId::MIN, StreamId::MAX).take(limit).map(|x| entry_reply(&x)).collect())));
    let mut groups = Vec::with_capacity(s.groups.len());
    for (name, g) in s.groups.iter() {
        let pending = g
            .pel()
            .take(limit)
            .map(|(id, n)| {
                Reply::Array(vec![
                    id_reply(id),
                    Reply::bulk(n.consumer.as_deref().unwrap_or(b"")),
                    Reply::Int(n.delivery_time),
                    Reply::Int(n.delivery_count as i64),
                ])
            })
            .collect();
        let consumers = g
            .consumers()
            .map(|(cname, c)| {
                let cpel = c
                    .pel()
                    .take(limit)
                    .map(|id| {
                        let n = g.nack(id).expect("consumer PEL entry is in the group PEL");
                        Reply::Array(vec![id_reply(id), Reply::Int(n.delivery_time), Reply::Int(n.delivery_count as i64)])
                    })
                    .collect();
                Reply::Map(vec![
                    kv("name", Reply::bulk(cname)),
                    kv("seen-time", Reply::Int(c.seen_time)),
                    kv("active-time", Reply::Int(c.active_time)),
                    kv("pel-count", Reply::Int(c.pending() as i64)),
                    kv("pending", Reply::Array(cpel)),
                ])
            })
            .collect();
        groups.push(Reply::Map(vec![
            kv("name", Reply::bulk(name)),
            kv("last-delivered-id", id_reply(g.last_id)),
            kv("entries-read", entries_read_reply(g)),
            kv("lag", lag_reply(e, g)),
            kv("pel-count", Reply::Int(g.pel_len() as i64)),
            kv("nacked-count", Reply::Int(g.nacked_count() as i64)),
            kv("pending", Reply::Array(pending)),
            kv("consumers", Reply::Array(consumers)),
        ]));
    }
    m.push(kv("groups", Reply::Array(groups)));
    Ok(Reply::Map(m))
}

pub fn xinfo_groups(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let Some(s) = cx.get_stream(&args[2])? else { return Err(NO_KEY) };
    let groups = s
        .groups
        .iter()
        .map(|(name, g)| {
            Reply::Map(vec![
                kv("name", Reply::bulk(name)),
                kv("consumers", Reply::Int(g.consumer_count() as i64)),
                kv("pending", Reply::Int(g.pel_len() as i64)),
                kv("last-delivered-id", id_reply(g.last_id)),
                kv("entries-read", entries_read_reply(g)),
                kv("lag", lag_reply(&s.entries, g)),
            ])
        })
        .collect();
    Ok(Reply::Array(groups))
}

pub fn xinfo_consumers(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let now = cx.now;
    let Some(s) = cx.get_stream(&args[2])? else { return Err(NO_KEY) };
    let Some(g) = s.groups.get(&args[3]) else {
        return Err(Error::msg(format!("NOGROUP No such consumer group '{}' for key name '{}'", show(&args[3]), show(&args[2]))));
    };
    let out = g
        .consumers()
        .map(|(name, c)| {
            let inactive = if c.active_time != -1 { now - c.active_time } else { -1 };
            Reply::Map(vec![
                kv("name", Reply::bulk(name)),
                kv("pending", Reply::Int(c.pending() as i64)),
                kv("idle", Reply::Int((now - c.seen_time).max(0))),
                kv("inactive", Reply::Int(inactive)),
            ])
        })
        .collect();
    Ok(Reply::Array(out))
}

pub fn xinfo_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "XINFO <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "CONSUMERS <key> <groupname>",
        "    Show consumers of <groupname>.",
        "GROUPS <key>",
        "    Show the stream consumer groups.",
        "STREAM <key> [FULL [COUNT <count>]",
        "    Show information about the stream.",
        "HELP",
        "    Print this help.",
    ]))
}

#[cfg(test)]
mod tests {
    use crate::client::Client;
    use crate::engine::{Effect, Engine};
    use bytes::Bytes;
    use redis_proto::Reply;

    fn run(e: &mut Engine, c: &mut Client, line: &str) -> Reply {
        let args: Vec<Bytes> = line.split(' ').map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
        e.call(c, &args).unwrap_reply()
    }

    fn effects(e: &mut Engine) -> Vec<String> {
        e.take_effects()
            .iter()
            .map(|x| x.args.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect::<Vec<_>>().join(" "))
            .collect()
    }

    #[test]
    fn xadd_propagates_concrete_id_and_exact_trim() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        let Reply::Bulk(id) = run(&mut e, &mut c, "XADD s * a 1") else { panic!() };
        let id = String::from_utf8_lossy(&id).into_owned();
        assert_eq!(effects(&mut e), vec![format!("XADD s {id} a 1")]);
        for i in 0..5 {
            run(&mut e, &mut c, &format!("XADD s 99999999999999-{i} a 1"));
        }
        e.take_effects();
        run(&mut e, &mut c, "XADD s MAXLEN ~ 2 99999999999999-9 a 1");
        assert_eq!(effects(&mut e), vec!["XADD s MAXLEN = 7 99999999999999-9 a 1".to_string()]);
        run(&mut e, &mut c, "XTRIM s MAXLEN 3");
        assert_eq!(effects(&mut e), vec!["XTRIM s MAXLEN 3".to_string()]);
        // Nothing removed: XTRIM isn't propagated.
        run(&mut e, &mut c, "XTRIM s MAXLEN 3");
        assert!(effects(&mut e).is_empty());
    }

    #[test]
    fn xreadgroup_propagates_claims_and_setid() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        run(&mut e, &mut c, "XADD s 1-0 a 1");
        run(&mut e, &mut c, "XADD s 2-0 a 2");
        run(&mut e, &mut c, "XGROUP CREATE s g 0");
        e.take_effects();
        run(&mut e, &mut c, "XREADGROUP GROUP g c COUNT 2 STREAMS s >");
        let fx = effects(&mut e);
        assert_eq!(fx.len(), 5, "{fx:?}");
        assert_eq!(fx[0], "MULTI");
        assert!(fx[1].starts_with("XCLAIM s g c 0 1-0 TIME "), "{fx:?}");
        assert!(fx[1].ends_with(" RETRYCOUNT 1 FORCE JUSTID LASTID 0-0"), "{fx:?}");
        assert!(fx[2].starts_with("XCLAIM s g c 0 2-0 TIME "), "{fx:?}");
        assert_eq!(fx[3], "XGROUP SETID s g 2-0 ENTRIESREAD 2");
        assert_eq!(fx[4], "EXEC");
        // Nothing new: nothing to propagate.
        assert_eq!(run(&mut e, &mut c, "XREADGROUP GROUP g c STREAMS s >"), Reply::NilArray);
        assert!(effects(&mut e).is_empty());
        // NOACK only moves the group forward.
        run(&mut e, &mut c, "XADD s 3-0 a 3");
        e.take_effects();
        run(&mut e, &mut c, "XREADGROUP GROUP g c NOACK STREAMS s >");
        assert_eq!(effects(&mut e), vec!["XGROUP SETID s g 3-0 ENTRIESREAD 3".to_string()]);
        // A new consumer reading history is created on the replica too.
        run(&mut e, &mut c, "XREADGROUP GROUP g c2 STREAMS s 0");
        assert_eq!(effects(&mut e), vec!["XGROUP CREATECONSUMER s g c2".to_string()]);
    }

    #[test]
    fn effects_replay_to_same_state() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        let mut log: Vec<Effect> = Vec::new();
        for line in [
            "XADD s 1-0 a 1",
            "XADD s * b 2",
            "XADD s MAXLEN ~ 1 * c 3",
            "XGROUP CREATE s g 0",
            "XREADGROUP GROUP g c1 COUNT 1 STREAMS s >",
            "XREADGROUP GROUP g c2 STREAMS s >",
            "XCLAIM s g c3 0 1-0",
            "XAUTOCLAIM s g c4 0 0 COUNT 1",
            "XACK s g 1-0",
            "XDEL s 1-0",
            "XNACK s g FAIL IDS 1 2-0",
            "XADD s IDMP p i * d 4",
            "XADD s IDMP p i * d 5",
            "XCFGSET s IDMP-MAXSIZE 50",
        ] {
            run(&mut e, &mut c, line);
            log.extend(e.take_effects());
        }
        let mut r = Engine::standalone();
        r.apply_all(&log);
        let mut rc = Client::new(2);
        for q in ["XRANGE s - +", "XPENDING s g", "XINFO GROUPS s", "XLEN s", "XINFO STREAM s"] {
            assert_eq!(run(&mut e, &mut c, q), run(&mut r, &mut rc, q), "{q}");
        }
    }
}
