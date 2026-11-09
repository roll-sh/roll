//! List commands.
//!
//! Encoding conversions happen where Redis does them: before adding elements (`grow`, with the
//! byte total Redis counts) and after removing them (`shrink`). Blocking variants try first and
//! block only when nothing can be served; when served they propagate their non-blocking form.

use super::*;
use crate::cmd;
use crate::engine::Mode;
use crate::notify;
use crate::types::list::Fill;
use crate::util::{fmt_i64, parse_i64};
use crate::value::{Kind, Str, ValueRef};

pub static COMMANDS: &[Command] = &[
    cmd!("lpush", -3, WRITE | DENYOOM | FAST, 1, 1, 1, lpush, List),
    cmd!("rpush", -3, WRITE | DENYOOM | FAST, 1, 1, 1, rpush, List),
    cmd!("lpushx", -3, WRITE | DENYOOM | FAST, 1, 1, 1, lpushx, List),
    cmd!("rpushx", -3, WRITE | DENYOOM | FAST, 1, 1, 1, rpushx, List),
    cmd!("lpop", -2, WRITE | FAST, 1, 1, 1, lpop, List),
    cmd!("rpop", -2, WRITE | FAST, 1, 1, 1, rpop, List),
    cmd!("llen", 2, READONLY | FAST, 1, 1, 1, llen, List),
    cmd!("lrange", 4, READONLY, 1, 1, 1, lrange, List),
    cmd!("lindex", 3, READONLY, 1, 1, 1, lindex, List),
    cmd!("lset", 4, WRITE | DENYOOM, 1, 1, 1, lset, List),
    cmd!("linsert", 5, WRITE | DENYOOM, 1, 1, 1, linsert, List),
    cmd!("lrem", 4, WRITE, 1, 1, 1, lrem, List),
    cmd!("ltrim", 4, WRITE, 1, 1, 1, ltrim, List),
    cmd!("lpos", -3, READONLY, 1, 1, 1, lpos, List),
    cmd!("lmove", 5, WRITE | DENYOOM, 1, 2, 1, lmove, List),
    cmd!("rpoplpush", 3, WRITE | DENYOOM, 1, 2, 1, rpoplpush, List),
    cmd!("lmovem", -5, WRITE | DENYOOM, 1, 2, 1, lmovem, List),
    cmd!("lmpop", -4, WRITE, keys = lmpop_keys, lmpop, List),
    cmd!("blpop", -3, WRITE | BLOCKING, 1, -2, 1, blpop, List),
    cmd!("brpop", -3, WRITE | BLOCKING, 1, -2, 1, brpop, List),
    cmd!("brpoplpush", 4, WRITE | DENYOOM | BLOCKING, 1, 2, 1, brpoplpush, List),
    cmd!("blmove", 6, WRITE | DENYOOM | BLOCKING, 1, 2, 1, blmove, List),
    cmd!("blmovem", -6, WRITE | DENYOOM | BLOCKING, 1, 2, 1, blmovem, List),
    cmd!("blmpop", -5, WRITE | BLOCKING, keys = blmpop_keys, blmpop, List),
];

const POSITIVE: &str = "ERR value is out of range, must be positive";
const COUNT_GT0: &str = "ERR count should be greater than 0";

fn b(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

fn bulk(v: Vec<u8>) -> Reply {
    Reply::Bulk(Bytes::from(v))
}

fn push_event(left: bool) -> &'static str {
    if left { "lpush" } else { "rpush" }
}

fn pop_event(left: bool) -> &'static str {
    if left { "lpop" } else { "rpop" }
}

/// `LEFT` → true, `RIGHT` → false.
fn side(a: &[u8]) -> Result<bool, Error> {
    if eq(a, "right") {
        Ok(false)
    } else if eq(a, "left") {
        Ok(true)
    } else {
        Err(SYNTAX)
    }
}

/// `getRangeLongFromObjectOrReply`: `msg` replaces both the parse and the range error.
pub(crate) fn long_in(a: &[u8], min: i64, max: i64, msg: Option<&'static str>) -> Result<i64, Error> {
    let Some(v) = parse_i64(a) else { return Err(msg.map_or(NOT_INT, Error::new)) };
    if v < min || v > max {
        return Err(match msg {
            Some(m) => Error::new(m),
            None => Error::msg(format!("ERR value is out of range, value must between {min} and {max}")),
        });
    }
    Ok(v)
}

fn fill(cx: &Cx) -> Fill {
    Fill { size: cx.cfg().list_max_listpack_size, packed_threshold: cx.eng.debug.packed_threshold, compress: cx.cfg().list_compress_depth as usize }
}

/// Length of the list at `key`: `None` if missing, WRONGTYPE for another type. No LRU touch.
fn list_len(cx: &mut Cx, key: &[u8]) -> Result<Option<usize>, Error> {
    match cx.peek(key) {
        None => Ok(None),
        Some(e) => match e.value() {
            ValueRef::List(l) => Ok(Some(l.len())),
            _ => Err(WRONGTYPE),
        },
    }
}

/// Bytes Redis adds up before pushing values popped from a list: integers popped from a
/// quicklist come back as integer objects, which its size check skips.
fn popped_bytes(vals: &[Vec<u8>], from_quicklist: bool) -> usize {
    vals.iter().map(|v| if from_quicklist && parse_i64(v).is_some() { 0 } else { v.len() }).sum()
}

/// A re-run blocked command (`Mode::Unblock`) is only served once one of its keys holds a list;
/// otherwise Redis keeps it blocked (a key of another type doesn't wake it).
fn still_waiting(cx: &mut Cx, keys: &[Bytes]) -> bool {
    cx.mode == Mode::Unblock && !keys.iter().any(|k| cx.peek(k).is_some_and(|e| e.value().kind() == Kind::List))
}

fn block(cx: &mut Cx, keys: &[Bytes], timeout_ms: i64, timeout_reply: Reply) -> Reply {
    let mut ks: Vec<Str> = Vec::with_capacity(keys.len());
    for k in keys {
        if !ks.iter().any(|x| x.as_slice() == &k[..]) {
            ks.push(Str::from_slice(k));
        }
    }
    cx.block_on(ks, timeout_ms, timeout_reply)
}

// ---- push / pop ----

fn push(cx: &mut Cx, args: &[Bytes], left: bool, xx: bool) -> CmdResult {
    let fill = fill(cx);
    let key = &args[1];
    let mut e = if xx {
        match cx.get_mut_typed(key, Kind::List)? {
            Some(e) => e,
            None => return Ok(Reply::Int(0)),
        }
    } else {
        cx.get_or_create(key, Kind::List)?
    };
    let vals = &args[2..];
    let l = e.list();
    l.grow(fill, vals.iter().map(|v| v.len()).sum(), vals.len());
    for v in vals {
        l.push(left, v);
    }
    let len = l.len();
    e.notify(notify::LIST, push_event(left));
    e.changes(vals.len());
    Ok(Reply::Int(len as i64))
}

/// LPUSH key element [element …]
pub fn lpush(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    push(cx, args, true, false)
}

/// RPUSH key element [element …]
pub fn rpush(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    push(cx, args, false, false)
}

/// LPUSHX key element [element …]
pub fn lpushx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    push(cx, args, true, true)
}

/// RPUSHX key element [element …]
pub fn rpushx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    push(cx, args, false, true)
}

/// Pop one element (`count == None`) or up to `count` from an existing, non-empty list, with the
/// pop event and the shrink conversion.
fn pop_from(cx: &mut Cx, key: &[u8], left: bool, count: Option<usize>) -> Reply {
    let fill = fill(cx);
    let mut e = cx.get_mut(key).expect("checked");
    let l = e.list();
    let (reply, n) = match count {
        None => (bulk(l.pop(left).expect("non-empty")), 1),
        Some(n) => {
            let vals = l.pop_n(left, n);
            let n = vals.len();
            (Reply::Array(vals.into_iter().map(bulk).collect()), n)
        }
    };
    l.shrink(fill);
    e.notify(notify::LIST, pop_event(left));
    e.changes(n);
    reply
}

fn pop(cx: &mut Cx, args: &[Bytes], left: bool) -> CmdResult {
    if args.len() > 3 {
        return Err(arity_error(cx.cmd.name));
    }
    let count = match args.get(2) {
        Some(a) => Some(long_in(a, 0, i64::MAX, Some(POSITIVE))? as usize),
        None => None,
    };
    match list_len(cx, &args[1])? {
        None => Ok(if count.is_some() { Reply::NilArray } else { Reply::Nil }),
        Some(_) if count == Some(0) => Ok(Reply::Array(Vec::new())),
        Some(_) => Ok(pop_from(cx, &args[1], left, count)),
    }
}

/// LPOP key [count]
pub fn lpop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    pop(cx, args, true)
}

/// RPOP key [count]
pub fn rpop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    pop(cx, args, false)
}

// ---- reads ----

/// LLEN key
pub fn llen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.get_list(&args[1])?.map_or(0, |l| l.len()) as i64))
}

/// LRANGE key start stop
pub fn lrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let start = int(&args[2])?;
    let end = int(&args[3])?;
    let Some(l) = cx.get_list(&args[1])? else { return Ok(Reply::Array(Vec::new())) };
    Ok(match range(start, end, l.len()) {
        None => Reply::Array(Vec::new()),
        Some((s, e)) => Reply::bulk_array(l.iter_from(s).take(e - s + 1)),
    })
}

/// LINDEX key index
pub fn lindex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let Some(l) = cx.get_list(&args[1])? else { return Ok(Reply::Nil) };
    let idx = int(&args[2])?;
    let i = if idx < 0 { l.len() as i64 + idx } else { idx };
    Ok(if i < 0 { Reply::Nil } else { l.get(i as usize).map_or(Reply::Nil, Reply::bulk) })
}

/// LPOS key element [RANK rank] [COUNT num-matches] [MAXLEN len]
pub fn lpos(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (mut rank, mut count, mut maxlen) = (1i64, -1i64, 0i64);
    let mut j = 3;
    while j < args.len() {
        let more = j + 1 < args.len();
        if eq(&args[j], "rank") && more {
            j += 1;
            rank = long_in(&args[j], -i64::MAX, i64::MAX, None)?;
            if rank == 0 {
                return Err(Error::new(
                    "ERR RANK can't be zero: use 1 to start from the first match, 2 from the second ... or use negative to start from the end of the list",
                ));
            }
        } else if eq(&args[j], "count") && more {
            j += 1;
            count = long_in(&args[j], 0, i64::MAX, Some("ERR COUNT can't be negative"))?;
        } else if eq(&args[j], "maxlen") && more {
            j += 1;
            maxlen = long_in(&args[j], 0, i64::MAX, Some("ERR MAXLEN can't be negative"))?;
        } else {
            return Err(SYNTAX);
        }
        j += 1;
    }
    let Some(l) = cx.get_list(&args[1])? else {
        return Ok(if count != -1 { Reply::Array(Vec::new()) } else { Reply::Nil });
    };
    let ele = &args[2][..];
    let llen = l.len();
    let back = rank < 0;
    let rank = rank.unsigned_abs();
    let maxlen = maxlen as usize;
    let mut out = Vec::new();
    let mut found = None;
    let mut matches = 0u64;
    let mut scan = |index: usize, v: &[u8]| -> bool {
        if maxlen != 0 && index >= maxlen {
            return false;
        }
        if v == ele {
            matches += 1;
            let at = if back { llen - index - 1 } else { index };
            if matches >= rank {
                if count == -1 {
                    found = Some(at);
                    return false;
                }
                out.push(Reply::Int(at as i64));
                if count != 0 && matches - rank + 1 >= count as u64 {
                    return false;
                }
            }
        }
        true
    };
    if back {
        for (i, v) in l.iter().rev().enumerate() {
            if !scan(i, v) {
                break;
            }
        }
    } else {
        for (i, v) in l.iter().enumerate() {
            if !scan(i, v) {
                break;
            }
        }
    }
    Ok(if count != -1 {
        Reply::Array(out)
    } else {
        found.map_or(Reply::Nil, |i| Reply::Int(i as i64))
    })
}

// ---- in-place edits ----

/// LSET key index element
pub fn lset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let fill = fill(cx);
    if !cx.check_type(&args[1], Kind::List)? {
        return Err(NO_KEY);
    }
    let idx = int(&args[2])?;
    let v = &args[3];
    let mut e = cx.get_mut(&args[1]).expect("checked");
    let l = e.list();
    // Redis converts before knowing whether the index is valid.
    l.grow(fill, v.len(), 1);
    let len = l.len() as i64;
    let i = if idx < 0 { len + idx } else { idx };
    if i < 0 || i >= len {
        e.unchanged();
        return Err(OUT_OF_RANGE);
    }
    l.set(i as usize, v);
    l.shrink(fill);
    e.notify(notify::LIST, "lset");
    Ok(Reply::Ok)
}

/// LINSERT key BEFORE | AFTER pivot element
pub fn linsert(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let after = if eq(&args[2], "after") {
        true
    } else if eq(&args[2], "before") {
        false
    } else {
        return Err(SYNTAX);
    };
    let fill = fill(cx);
    if !cx.check_type(&args[1], Kind::List)? {
        return Ok(Reply::Int(0));
    }
    let mut e = cx.get_mut(&args[1]).expect("checked");
    let l = e.list();
    l.grow(fill, args[4].len(), 1);
    let Some(i) = l.iter().position(|x| x == &args[3][..]) else {
        e.unchanged();
        return Ok(Reply::Int(-1));
    };
    l.insert(i, after, &args[4]);
    let len = l.len();
    e.notify(notify::LIST, "linsert");
    Ok(Reply::Int(len as i64))
}

/// LREM key count element
pub fn lrem(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let n = long_in(&args[2], -i64::MAX, i64::MAX, None)?;
    let fill = fill(cx);
    if !cx.check_type(&args[1], Kind::List)? {
        return Ok(Reply::Int(0));
    }
    let mut e = cx.get_mut(&args[1]).expect("checked");
    let l = e.list();
    let removed = l.remove_matching(&args[3], n.unsigned_abs() as usize, n < 0);
    if removed == 0 {
        e.unchanged();
    } else {
        l.shrink(fill);
        e.notify(notify::LIST, "lrem");
        e.changes(removed);
    }
    Ok(Reply::Int(removed as i64))
}

/// LTRIM key start stop
pub fn ltrim(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let start = int(&args[2])?;
    let end = int(&args[3])?;
    let fill = fill(cx);
    if !cx.check_type(&args[1], Kind::List)? {
        return Ok(Reply::Ok);
    }
    let mut e = cx.get_mut(&args[1]).expect("checked");
    let l = e.list();
    let len = l.len() as i64;
    let s = (if start < 0 { len + start } else { start }).max(0);
    let mut t = if end < 0 { len + end } else { end };
    let (ltrim, rtrim) = if s > t || s >= len {
        (len, 0)
    } else {
        if t >= len {
            t = len - 1;
        }
        (s, len - t - 1)
    };
    l.remove_range(0, ltrim as usize);
    let rest = l.len();
    l.remove_range(rest - rtrim as usize, rtrim as usize);
    l.shrink(fill);
    e.notify(notify::LIST, "ltrim");
    e.changes((ltrim + rtrim) as usize);
    Ok(Reply::Ok)
}

// ---- moves ----

/// LMOVE semantics: pop one element from `src` and push it to `dst`. `None` if `src` is missing.
fn move_one(cx: &mut Cx, src: &[u8], dst: &[u8], from_left: bool, to_left: bool) -> Result<Option<Vec<u8>>, Error> {
    let fill = fill(cx);
    if !cx.check_type(src, Kind::List)? {
        return Ok(None);
    }
    if src == dst {
        let mut e = cx.get_mut(src).expect("checked");
        let l = e.list();
        let quick = l.is_quicklist();
        let v = l.pop(from_left).expect("non-empty");
        l.grow(fill, popped_bytes(std::slice::from_ref(&v), quick), 1);
        l.push(to_left, &v);
        l.shrink(fill);
        e.notify(notify::LIST, push_event(to_left));
        e.notify(notify::LIST, pop_event(from_left));
        e.changes(1);
        return Ok(Some(v));
    }
    cx.check_type(dst, Kind::List)?;
    let (v, quick) = match cx.peek(src).map(|e| e.value()) {
        Some(ValueRef::List(l)) => ((if from_left { l.first() } else { l.last() }).expect("non-empty").to_vec(), l.is_quicklist()),
        _ => unreachable!("checked"),
    };
    {
        let mut d = cx.get_or_create(dst, Kind::List)?;
        let l = d.list();
        l.grow(fill, popped_bytes(std::slice::from_ref(&v), quick), 1);
        l.push(to_left, &v);
        d.notify(notify::LIST, push_event(to_left));
    }
    let mut s = cx.get_mut(src).expect("checked");
    let l = s.list();
    let at = if from_left { 0 } else { l.len() - 1 };
    l.remove_range(at, 1);
    l.shrink(fill);
    s.notify(notify::LIST, pop_event(from_left));
    s.changes(1);
    Ok(Some(v))
}

/// LMOVE source destination LEFT | RIGHT LEFT | RIGHT
pub fn lmove(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let from = side(&args[3])?;
    let to = side(&args[4])?;
    Ok(move_one(cx, &args[1], &args[2], from, to)?.map_or(Reply::Nil, bulk))
}

/// RPOPLPUSH source destination
pub fn rpoplpush(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(move_one(cx, &args[1], &args[2], false, true)?.map_or(Reply::Nil, bulk))
}

/// Parsed `[COUNT n | EXACTLY n] [OBO | BULK]` of [B]LMOVEM.
struct MoveM {
    exactly: bool,
    count: usize,
    obo: bool,
}

/// The trailer is either absent or exactly `COUNT|EXACTLY n OBO|BULK`.
fn lmovem_opts(args: &[Bytes], at: usize) -> Result<MoveM, Error> {
    if at == args.len() {
        return Ok(MoveM { exactly: false, count: 1, obo: false });
    }
    if args.len() - at != 3 {
        return Err(SYNTAX);
    }
    let exactly = if eq(&args[at], "count") {
        false
    } else if eq(&args[at], "exactly") {
        true
    } else {
        return Err(SYNTAX);
    };
    let count = long_in(&args[at + 1], 1, i64::MAX, Some(COUNT_GT0))? as usize;
    let obo = if eq(&args[at + 2], "obo") {
        true
    } else if eq(&args[at + 2], "bulk") {
        false
    } else {
        return Err(SYNTAX);
    };
    Ok(MoveM { exactly, count, obo })
}

/// LMOVEM semantics: move a block of elements. `None` when nothing can be moved.
fn move_many(cx: &mut Cx, src: &[u8], dst: &[u8], from_left: bool, to_left: bool, m: &MoveM) -> Result<Option<Vec<Vec<u8>>>, Error> {
    let fill = fill(cx);
    let srclen = list_len(cx, src)?.unwrap_or(0);
    let n = if m.exactly {
        if srclen < m.count {
            return Ok(None);
        }
        m.count
    } else {
        m.count.min(srclen)
    };
    if n == 0 {
        return Ok(None);
    }
    cx.check_type(dst, Kind::List)?;
    // Destination order: OBO pushes one by one (reversed at the head), BULK keeps source order.
    let reverse = if m.obo { to_left } else { !from_left };
    let push_all = |l: &mut crate::types::List, vals: &[Vec<u8>], quick: bool| {
        l.grow(fill, popped_bytes(vals, quick), vals.len());
        if to_left {
            vals.iter().rev().for_each(|v| l.push_front(v));
        } else {
            vals.iter().for_each(|v| l.push_back(v));
        }
    };
    if src == dst {
        let mut e = cx.get_mut(src).expect("checked");
        let l = e.list();
        let quick = l.is_quicklist();
        let mut vals = l.pop_n(from_left, n);
        if reverse {
            vals.reverse();
        }
        push_all(l, &vals, quick);
        // Unlike LMOVE, an in-place LMOVEM rotation reports the pop first.
        e.notify(notify::LIST, pop_event(from_left));
        e.notify(notify::LIST, push_event(to_left));
        e.changes(n);
        return Ok(Some(vals));
    }
    let (mut vals, quick) = match cx.peek(src).map(|e| e.value()) {
        Some(ValueRef::List(l)) => {
            let vals: Vec<Vec<u8>> =
                if from_left { l.iter().take(n).map(<[u8]>::to_vec).collect() } else { l.iter().rev().take(n).map(<[u8]>::to_vec).collect() };
            (vals, l.is_quicklist())
        }
        _ => unreachable!("checked"),
    };
    if reverse {
        vals.reverse();
    }
    {
        let mut d = cx.get_or_create(dst, Kind::List)?;
        push_all(d.list(), &vals, quick);
        d.notify(notify::LIST, push_event(to_left));
    }
    let mut s = cx.get_mut(src).expect("checked");
    let l = s.list();
    let at = if from_left { 0 } else { l.len() - n };
    l.remove_range(at, n);
    l.shrink(fill);
    s.notify(notify::LIST, pop_event(from_left));
    s.changes(n);
    Ok(Some(vals))
}

/// LMOVEM source destination LEFT | RIGHT LEFT | RIGHT [COUNT n | EXACTLY n OBO | BULK]
pub fn lmovem(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let from = side(&args[3])?;
    let to = side(&args[4])?;
    let m = lmovem_opts(args, 5)?;
    Ok(match move_many(cx, &args[1], &args[2], from, to, &m)? {
        Some(vals) => Reply::Array(vals.into_iter().map(bulk).collect()),
        None => Reply::NilArray,
    })
}

// ---- multi-key pops ----

fn mpop_keys(args: &[Bytes], numkeys_at: usize) -> Vec<usize> {
    let Some(n) = args.get(numkeys_at).and_then(|a| parse_i64(a)) else { return vec![] };
    (0..n.max(0) as usize).map(|i| numkeys_at + 1 + i).take_while(|&i| i < args.len()).collect()
}

fn lmpop_keys(args: &[Bytes]) -> Vec<usize> {
    mpop_keys(args, 1)
}

fn blmpop_keys(args: &[Bytes]) -> Vec<usize> {
    mpop_keys(args, 2)
}

/// Parsed `numkeys key… LEFT|RIGHT [COUNT n]`: (keys, left, count).
fn mpop_args(args: &[Bytes], numkeys_at: usize) -> Result<(&[Bytes], bool, usize), Error> {
    let n = long_in(&args[numkeys_at], 1, i64::MAX, Some("ERR numkeys should be greater than 0"))? as usize;
    let where_at = numkeys_at.saturating_add(n).saturating_add(1);
    if where_at >= args.len() {
        return Err(SYNTAX);
    }
    let left = side(&args[where_at])?;
    let mut count = None;
    let mut j = where_at + 1;
    while j < args.len() {
        if count.is_none() && eq(&args[j], "count") && j + 1 < args.len() {
            j += 1;
            count = Some(long_in(&args[j], 1, i64::MAX, Some(COUNT_GT0))? as usize);
        } else {
            return Err(SYNTAX);
        }
        j += 1;
    }
    Ok((&args[numkeys_at + 1..where_at], left, count.unwrap_or(1)))
}

/// Pop from the first non-empty list among `keys`: `[key, [elements]]`, propagated as
/// `LPOP|RPOP key n`.
fn mpop(cx: &mut Cx, keys: &[Bytes], left: bool, count: usize) -> Result<Option<Reply>, Error> {
    for k in keys {
        let Some(len) = list_len(cx, k)? else { continue };
        let reply = pop_from(cx, k, left, Some(count));
        cx.propagate_as(vec![b(if left { "LPOP" } else { "RPOP" }), k.clone(), Bytes::from(fmt_i64(count.min(len) as i64))]);
        return Ok(Some(Reply::Array(vec![Reply::Bulk(k.clone()), reply])));
    }
    Ok(None)
}

/// LMPOP numkeys key [key …] LEFT | RIGHT [COUNT count]
pub fn lmpop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (keys, left, count) = mpop_args(args, 1)?;
    Ok(mpop(cx, keys, left, count)?.unwrap_or(Reply::NilArray))
}

// ---- blocking ----

fn bpop(cx: &mut Cx, args: &[Bytes], left: bool) -> CmdResult {
    let timeout = timeout_secs(&args[args.len() - 1])?;
    let keys = &args[1..args.len() - 1];
    if still_waiting(cx, keys) {
        return Ok(block(cx, keys, timeout, Reply::NilArray));
    }
    for k in keys {
        if list_len(cx, k)?.is_none() {
            continue;
        }
        let v = pop_from(cx, k, left, None);
        cx.propagate_as(vec![b(if left { "LPOP" } else { "RPOP" }), k.clone()]);
        return Ok(Reply::Array(vec![Reply::Bulk(k.clone()), v]));
    }
    Ok(block(cx, keys, timeout, Reply::NilArray))
}

/// BLPOP key [key …] timeout
pub fn blpop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    bpop(cx, args, true)
}

/// BRPOP key [key …] timeout
pub fn brpop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    bpop(cx, args, false)
}

/// BLMPOP timeout numkeys key [key …] LEFT | RIGHT [COUNT count]
pub fn blmpop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (keys, left, count) = mpop_args(args, 2)?;
    let timeout = timeout_secs(&args[1])?;
    if still_waiting(cx, keys) {
        return Ok(block(cx, keys, timeout, Reply::NilArray));
    }
    match mpop(cx, keys, left, count)? {
        Some(r) => Ok(r),
        None => Ok(block(cx, keys, timeout, Reply::NilArray)),
    }
}

/// BLMOVE / BRPOPLPUSH. Times out with a null array, but answers a null bulk when it can't block
/// (MULTI, scripts), like Redis.
fn bmove(cx: &mut Cx, args: &[Bytes], from_left: bool, to_left: bool, timeout: i64, rewrite: Vec<Bytes>) -> CmdResult {
    let src = &args[1..2];
    if still_waiting(cx, src) {
        return Ok(block(cx, src, timeout, Reply::NilArray));
    }
    match move_one(cx, &args[1], &args[2], from_left, to_left)? {
        Some(v) => {
            cx.propagate_as(rewrite);
            Ok(bulk(v))
        }
        None if !cx.can_block() => Ok(Reply::Nil),
        None => Ok(block(cx, src, timeout, Reply::NilArray)),
    }
}

/// BLMOVE source destination LEFT | RIGHT LEFT | RIGHT timeout
pub fn blmove(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let from = side(&args[3])?;
    let to = side(&args[4])?;
    let timeout = timeout_secs(&args[5])?;
    let rewrite = vec![b("LMOVE"), args[1].clone(), args[2].clone(), args[3].clone(), args[4].clone()];
    bmove(cx, args, from, to, timeout, rewrite)
}

/// BRPOPLPUSH source destination timeout
pub fn brpoplpush(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let timeout = timeout_secs(&args[3])?;
    let rewrite = vec![b("RPOPLPUSH"), args[1].clone(), args[2].clone()];
    bmove(cx, args, false, true, timeout, rewrite)
}

/// BLMOVEM source destination LEFT | RIGHT LEFT | RIGHT timeout [COUNT n | EXACTLY n OBO | BULK]
///
/// EXACTLY waits until the source holds `n` elements. Served moves propagate as
/// `LMOVEM … EXACTLY moved OBO|BULK`.
pub fn blmovem(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let from = side(&args[3])?;
    let to = side(&args[4])?;
    let timeout = timeout_secs(&args[5])?;
    let m = lmovem_opts(args, 6)?;
    let src = &args[1..2];
    if still_waiting(cx, src) {
        return Ok(block(cx, src, timeout, Reply::NilArray));
    }
    let needed = if m.exactly { m.count } else { 1 };
    if list_len(cx, &args[1])?.unwrap_or(0) < needed {
        return Ok(block(cx, src, timeout, Reply::NilArray));
    }
    let vals = move_many(cx, &args[1], &args[2], from, to, &m)?.expect("source has enough elements");
    cx.propagate_as(vec![
        b("LMOVEM"),
        args[1].clone(),
        args[2].clone(),
        args[3].clone(),
        args[4].clone(),
        b("EXACTLY"),
        Bytes::from(fmt_i64(vals.len() as i64)),
        b(if m.obo { "OBO" } else { "BULK" }),
    ]);
    Ok(Reply::Array(vals.into_iter().map(bulk).collect()))
}

#[cfg(test)]
mod tests {
    use crate::client::Client;
    use crate::engine::{Engine, Outcome};
    use bytes::Bytes;
    use redis_proto::Reply;

    fn run(e: &mut Engine, c: &mut Client, line: &str) -> Reply {
        let args: Vec<Bytes> = line.split_whitespace().map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
        match e.call(c, &args) {
            Outcome::Reply(r) => r,
            Outcome::Blocked => Reply::Status("BLOCKED".into()),
        }
    }

    fn bulks(items: &[&str]) -> Reply {
        Reply::Array(items.iter().map(Reply::bulk).collect())
    }

    #[test]
    fn push_pop_range() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        assert_eq!(run(&mut e, &mut c, "RPUSH l a b c"), Reply::Int(3));
        assert_eq!(run(&mut e, &mut c, "LPUSH l z"), Reply::Int(4));
        assert_eq!(run(&mut e, &mut c, "LRANGE l 0 -1"), bulks(&["z", "a", "b", "c"]));
        assert_eq!(run(&mut e, &mut c, "RPOP l 2"), bulks(&["c", "b"]));
        assert_eq!(run(&mut e, &mut c, "LPOP l"), Reply::bulk("z"));
        assert_eq!(run(&mut e, &mut c, "LPOP l 0"), Reply::Array(vec![]));
        assert_eq!(run(&mut e, &mut c, "LPOP l 5"), bulks(&["a"]));
        assert_eq!(run(&mut e, &mut c, "EXISTS l"), Reply::Int(0));
        assert_eq!(run(&mut e, &mut c, "LPOP l 5"), Reply::NilArray);
        assert_eq!(run(&mut e, &mut c, "LPOP l"), Reply::Nil);
        assert_eq!(run(&mut e, &mut c, "LPUSHX l a"), Reply::Int(0));
    }

    #[test]
    fn edits() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        run(&mut e, &mut c, "RPUSH l a b c b a");
        assert_eq!(run(&mut e, &mut c, "LINSERT l BEFORE b x"), Reply::Int(6));
        assert_eq!(run(&mut e, &mut c, "LINSERT l AFTER nope x"), Reply::Int(-1));
        assert_eq!(run(&mut e, &mut c, "LREM l -1 b"), Reply::Int(1));
        assert_eq!(run(&mut e, &mut c, "LRANGE l 0 -1"), bulks(&["a", "x", "b", "c", "a"]));
        assert_eq!(run(&mut e, &mut c, "LSET l -1 z"), Reply::Ok);
        assert!(run(&mut e, &mut c, "LSET l 9 z").is_error());
        assert_eq!(run(&mut e, &mut c, "LTRIM l 1 -2"), Reply::Ok);
        assert_eq!(run(&mut e, &mut c, "LRANGE l 0 -1"), bulks(&["x", "b", "c"]));
        assert_eq!(run(&mut e, &mut c, "LPOS l c"), Reply::Int(2));
        assert_eq!(run(&mut e, &mut c, "LINDEX l -3"), Reply::bulk("x"));
    }

    #[test]
    fn moves_and_propagation() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        run(&mut e, &mut c, "RPUSH s 1 2 3 4");
        e.take_effects();
        assert_eq!(run(&mut e, &mut c, "LMOVE s d RIGHT LEFT"), Reply::bulk("4"));
        assert_eq!(run(&mut e, &mut c, "LMOVEM s d LEFT RIGHT COUNT 2 BULK"), bulks(&["1", "2"]));
        assert_eq!(run(&mut e, &mut c, "LRANGE d 0 -1"), bulks(&["4", "1", "2"]));
        assert_eq!(run(&mut e, &mut c, "LMPOP 2 x d RIGHT COUNT 9"), Reply::Array(vec![Reply::bulk("d"), bulks(&["2", "1", "4"])]));
        let fx: Vec<String> = e.take_effects().iter().map(|f| f.args.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect::<Vec<_>>().join(" ")).collect();
        assert_eq!(fx, ["LMOVE s d RIGHT LEFT", "LMOVEM s d LEFT RIGHT COUNT 2 BULK", "RPOP d 3"]);
    }

    #[test]
    fn blocking_fifo() {
        let mut e = Engine::standalone();
        let mut a = Client::new(1);
        let mut b = Client::new(2);
        let mut p = Client::new(3);
        assert_eq!(run(&mut e, &mut a, "BLPOP l 0"), Reply::Status("BLOCKED".into()));
        assert_eq!(run(&mut e, &mut b, "BLPOP l 0"), Reply::Status("BLOCKED".into()));
        // A string at the key doesn't wake list waiters.
        run(&mut e, &mut p, "SET l x");
        assert_eq!(e.blocked_clients(), 2);
        run(&mut e, &mut p, "DEL l");
        e.take_effects();
        run(&mut e, &mut p, "RPUSH l 1");
        assert!(!e.is_blocked(1));
        assert!(e.is_blocked(2));
        let fx: Vec<Vec<Bytes>> = e.take_effects().into_iter().map(|f| f.args).collect();
        assert_eq!(fx[1], vec![Bytes::from("LPOP"), Bytes::from("l")]);
        assert_eq!(run(&mut e, &mut p, "EXISTS l"), Reply::Int(0));
    }

    #[test]
    fn multi_does_not_block() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        run(&mut e, &mut c, "MULTI");
        run(&mut e, &mut c, "BLPOP l 0");
        run(&mut e, &mut c, "BLMOVE l d LEFT LEFT 0");
        assert_eq!(run(&mut e, &mut c, "EXEC"), Reply::Array(vec![Reply::NilArray, Reply::Nil]));
    }
}
