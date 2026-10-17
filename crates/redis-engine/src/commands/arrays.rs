//! Array commands (Redis 8.10 `t_array.c`): ARSET/ARMSET/ARGET/ARMGET, ARDEL/ARDELRANGE,
//! ARLEN/ARCOUNT, ARGETRANGE, ARSCAN, ARGREP, AROP, the ring-buffer family (ARINSERT, ARRING,
//! ARNEXT, ARSEEK, ARLASTITEMS) and ARINFO.
//!
//! Argument validation happens in Redis' order (some commands look the key up first, some only
//! after every index parsed), and writes propagate verbatim: the insert cursor is part of the
//! value, so replicas replaying ARINSERT/ARRING reach the same state.

use super::*;
use crate::cx::KeyMut;
use crate::decimal::Ld;
use crate::notify;
use crate::tre;
use crate::types::array::{self, Array, Decoded, Limits, Val};
use crate::util::{cstr, glob_match, parse_i64, string2ull};
use crate::value::Kind;
use crate::cmd;

pub static COMMANDS: &[Command] = &[
    cmd!("arset", -4, WRITE | DENYOOM | FAST, 1, 1, 1, arset, Array),
    cmd!("armset", -4, WRITE | DENYOOM | FAST, 1, 1, 1, armset, Array),
    cmd!("arget", 3, READONLY | FAST, 1, 1, 1, arget, Array),
    cmd!("armget", -3, READONLY | FAST, 1, 1, 1, armget, Array),
    cmd!("ardel", -3, WRITE | FAST, 1, 1, 1, ardel, Array),
    cmd!("ardelrange", -4, WRITE, 1, 1, 1, ardelrange, Array),
    cmd!("arlen", 2, READONLY | FAST, 1, 1, 1, arlen, Array),
    cmd!("arcount", 2, READONLY | FAST, 1, 1, 1, arcount, Array),
    cmd!("argetrange", 4, READONLY, 1, 1, 1, argetrange, Array),
    cmd!("arscan", -4, READONLY, 1, 1, 1, arscan, Array),
    cmd!("argrep", -6, READONLY, 1, 1, 1, argrep, Array),
    cmd!("arop", -5, READONLY, 1, 1, 1, arop, Array),
    cmd!("arinsert", -3, WRITE | DENYOOM | FAST, 1, 1, 1, arinsert, Array),
    cmd!("arring", -4, WRITE | DENYOOM, 1, 1, 1, arring, Array),
    cmd!("arnext", 2, READONLY | FAST, 1, 1, 1, arnext, Array),
    cmd!("arseek", 3, WRITE | FAST, 1, 1, 1, arseek, Array),
    cmd!("arlastitems", -3, READONLY, 1, 1, 1, arlastitems, Array),
    cmd!("arinfo", -2, READONLY, 1, 1, 1, arinfo, Array),
];

const INVALID_INDEX: Error = Error::new("ERR invalid array index");
const INDEX_OVERFLOW: Error = Error::new("ERR array index overflow");
const INSERT_OVERFLOW: Error = Error::new("ERR insert index overflow");
const LIMIT_POSITIVE: Error = Error::new("ERR LIMIT must be positive");
const GETRANGE_MAX: u64 = 1_000_000;
const GREP_MAX_PREDICATES: usize = 250;
const GREP_MAX_RE_LEN: usize = 2048;

/// `getArrayIndexFromObject`: `string2ull` on the C string; `u64::MAX` only when allowed.
fn parse_index(a: &[u8], allow_max: bool) -> Option<u64> {
    let v = string2ull(cstr(a))?;
    (v != u64::MAX || allow_max).then_some(v)
}

fn index(a: &[u8]) -> Result<u64, Error> {
    parse_index(a, false).ok_or(INVALID_INDEX)
}

fn limits(cx: &Cx) -> Limits {
    Limits { kmax: cx.cfg().array_sparse_kmax, kmin: cx.cfg().array_sparse_kmin }
}

/// `addReplyUnsignedLongLong`: a big number past `i64::MAX`.
fn unsigned(v: u64) -> Reply {
    if v <= i64::MAX as u64 { Reply::Int(v as i64) } else { Reply::BigNum(v.to_string()) }
}

fn value(v: Option<&Val>) -> Reply {
    match v {
        Some(v) => Reply::Bulk(Bytes::from(v.text().into_owned())),
        None => Reply::Nil,
    }
}

/// `strcasecmp` against a keyword (the argument as a C string).
fn kw(a: &[u8], s: &str) -> bool {
    cstr(a).eq_ignore_ascii_case(s.as_bytes())
}

/// `lookupArrayForWriteOrReply`: the key's array, created (with the current slice size) if
/// missing.
fn write_array<'a>(cx: &'a mut Cx, key: &[u8]) -> Result<KeyMut<'a>, Error> {
    let size = cx.cfg().array_slice_size;
    let mut e = cx.get_or_create(key, Kind::Array)?;
    if e.array().is_empty() {
        *e.array() = Array::new(size);
    }
    Ok(e)
}

/// ARGET key index
pub fn arget(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let a = cx.get_array(&args[1])?;
    let idx = index(&args[2])?;
    Ok(value(a.and_then(|a| a.get(idx))))
}

/// ARMGET key index [index ...]
pub fn armget(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let a = cx.get_array(&args[1])?;
    let idxs = args[2..].iter().map(|x| index(x)).collect::<Result<Vec<_>, _>>()?;
    Ok(Reply::Array(idxs.into_iter().map(|i| value(a.and_then(|a| a.get(i)))).collect()))
}

/// ARSET key index value [value ...]
pub fn arset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let start = index(&args[2])?;
    let n = (args.len() - 3) as u64;
    let last = start.checked_add(n - 1).filter(|&l| l != u64::MAX).ok_or(INDEX_OVERFLOW)?;
    let lim = limits(cx);
    let mut e = write_array(cx, &args[1])?;
    let a = e.array();
    let before = a.count();
    if n > 1 {
        a.promote_for_range_set(start, last, lim);
    }
    for (i, v) in args[3..].iter().enumerate() {
        a.set(start + i as u64, Val::encode(v), lim);
    }
    let added = a.count() - before;
    e.notify(notify::ARRAY, "arset");
    Ok(Reply::Int(added as i64))
}

/// ARMSET key index value [index value ...]
pub fn armset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !(args.len() - 2).is_multiple_of(2) {
        return Err(arity_error("armset"));
    }
    let idxs = args[2..].iter().step_by(2).map(|x| index(x)).collect::<Result<Vec<_>, _>>()?;
    let lim = limits(cx);
    let mut e = write_array(cx, &args[1])?;
    let a = e.array();
    let before = a.count();
    for (i, idx) in idxs.into_iter().enumerate() {
        a.set(idx, Val::encode(&args[3 + 2 * i]), lim);
    }
    let added = a.count() - before;
    e.notify(notify::ARRAY, "armset");
    Ok(Reply::Int(added as i64))
}

/// ARDEL key index [index ...]
pub fn ardel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let idxs = args[2..].iter().map(|x| index(x)).collect::<Result<Vec<_>, _>>()?;
    let lim = limits(cx);
    let Some(mut e) = cx.get_mut_typed(&args[1], Kind::Array)? else { return Ok(Reply::Int(0)) };
    let a = e.array();
    let deleted = idxs.into_iter().filter(|&i| a.del(i, lim)).count();
    if deleted > 0 {
        e.notify(notify::ARRAY, "ardel");
    } else {
        e.unchanged();
    }
    Ok(Reply::Int(deleted as i64))
}

/// ARDELRANGE key start end [start end ...]
pub fn ardelrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !(args.len() - 2).is_multiple_of(2) {
        return Err(arity_error("ardelrange"));
    }
    let mut ranges = Vec::new();
    for p in args[2..].chunks(2) {
        let (s, e) = (index(&p[0])?, index(&p[1])?);
        ranges.push((s.min(e), s.max(e)));
    }
    let lim = limits(cx);
    let Some(mut e) = cx.get_mut_typed(&args[1], Kind::Array)? else { return Ok(Reply::Int(0)) };
    let a = e.array();
    let deleted: u64 = ranges.into_iter().map(|(lo, hi)| a.delete_range(lo, hi, lim)).sum();
    if deleted > 0 {
        e.notify(notify::ARRAY, "ardelrange");
    } else {
        e.unchanged();
    }
    Ok(unsigned(deleted))
}

/// ARLEN key
pub fn arlen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(unsigned(cx.get_array(&args[1])?.map_or(0, |a| a.len())))
}

/// ARCOUNT key
pub fn arcount(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(unsigned(cx.get_array(&args[1])?.map_or(0, |a| a.count())))
}

/// ARGETRANGE key start end: every position, holes as nulls (reversed when start > end).
pub fn argetrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (start, end) = (index(&args[2])?, index(&args[3])?);
    let a = cx.get_array(&args[1])?;
    let (lo, hi) = (start.min(end), start.max(end));
    if hi - lo >= GETRANGE_MAX {
        return Err(Error::msg(format!("ERR range exceeds maximum of {GETRANGE_MAX} items")));
    }
    let get = |i: u64| value(a.and_then(|a| a.get(i)));
    Ok(Reply::Array(if start > end { (lo..=hi).rev().map(get).collect() } else { (lo..=hi).map(get).collect() }))
}

/// `LIMIT count` for ARSCAN/ARGREP.
fn parse_limit(a: &[u8]) -> Result<u64, Error> {
    let n = int(a)?;
    if n <= 0 { Err(LIMIT_POSITIVE) } else { Ok(n as u64) }
}

/// ARSCAN key start end [LIMIT count]: existing elements as [index, value] pairs.
pub fn arscan(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (start, end) = (index(&args[2])?, index(&args[3])?);
    let limit = match args.len() {
        6 if kw(&args[4], "limit") => parse_limit(&args[5])?,
        6 => return Err(SYNTAX),
        4 => u64::MAX,
        _ => return Err(arity_error("arscan")),
    };
    let Some(a) = cx.get_array(&args[1])? else { return Ok(Reply::Array(vec![])) };
    Ok(Reply::Array(a.scan(start, end).take(limit.min(usize::MAX as u64) as usize).map(|(i, v)| Reply::Array(vec![unsigned(i), value(Some(v))])).collect()))
}

// ---- ARGREP ----

enum Pred {
    Exact(Bytes),
    Match(Bytes),
    Glob(Bytes),
    Re(Bytes, Option<tre::Regex>),
}

/// `-`, `+` or an index.
#[derive(Clone, Copy)]
enum Bound {
    Start,
    End,
    At(u64),
}

fn parse_bound(a: &[u8]) -> Result<Bound, Error> {
    match a {
        b"-" => Ok(Bound::Start),
        b"+" => Ok(Bound::End),
        _ => index(a).map(Bound::At),
    }
}

fn eq_nocase(a: &[u8], b: &[u8], nocase: bool) -> bool {
    if nocase { a.eq_ignore_ascii_case(b) } else { a == b }
}

fn contains(hay: &[u8], needle: &[u8], nocase: bool) -> bool {
    needle.is_empty() || (needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| eq_nocase(w, needle, nocase)))
}

/// ARGREP key start end (EXACT s | MATCH s | GLOB p | RE p) ... [AND | OR] [LIMIT n]
/// [WITHVALUES] [NOCASE]
pub fn argrep(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (start, end) = (parse_bound(&args[2])?, parse_bound(&args[3])?);
    let mut preds = Vec::new();
    let (mut and, mut limit, mut withvalues, mut nocase) = (false, u64::MAX, false, false);
    let mut i = 4;
    while i < args.len() {
        let t = &args[i];
        let pred = [("exact", 0), ("match", 1), ("glob", 2), ("re", 3)].into_iter().find(|(k, _)| kw(t, k));
        if let Some((_, ty)) = pred {
            let Some(p) = args.get(i + 1) else { return Err(SYNTAX) };
            if preds.len() >= GREP_MAX_PREDICATES {
                return Err(Error::msg(format!("ERR too many predicates, maximum is {GREP_MAX_PREDICATES}")));
            }
            if ty == 3 && p.len() > GREP_MAX_RE_LEN {
                return Err(Error::msg(format!("ERR regular expression is too long, maximum is {GREP_MAX_RE_LEN} bytes")));
            }
            preds.push(match ty {
                0 => Pred::Exact(p.clone()),
                1 => Pred::Match(p.clone()),
                2 => Pred::Glob(p.clone()),
                _ => Pred::Re(p.clone(), None),
            });
            i += 2;
        } else if kw(t, "limit") {
            let Some(n) = args.get(i + 1) else { return Err(SYNTAX) };
            limit = parse_limit(n)?;
            i += 2;
        } else if kw(t, "withvalues") {
            withvalues = true;
            i += 1;
        } else if kw(t, "nocase") {
            nocase = true;
            i += 1;
        } else if kw(t, "and") || kw(t, "or") {
            and = kw(t, "and");
            i += 1;
        } else {
            return Err(SYNTAX);
        }
    }
    if preds.is_empty() {
        return Err(SYNTAX);
    }
    for p in &mut preds {
        if let Pred::Re(pat, re) = p {
            if pat.is_empty() {
                return Err(Error::new("ERR regular expression is empty"));
            }
            let r = tre::Regex::new(pat, nocase).map_err(|e| Error::msg(format!("ERR invalid regular expression: {}", e.message())))?;
            if r.has_backrefs() {
                return Err(Error::new("ERR regular expression backreferences are not supported"));
            }
            *re = Some(r);
        }
    }
    let Some(a) = cx.get_array(&args[1])? else { return Ok(Reply::Array(vec![])) };
    let len = a.len();
    if len == 0 || a.count() == 0 {
        return Ok(Reply::Array(vec![]));
    }
    let resolve = |b: Bound| match b {
        Bound::Start => 0,
        Bound::End => len - 1,
        Bound::At(i) => i,
    };
    let matches = |data: &[u8]| {
        let one = |p: &Pred| match p {
            Pred::Exact(s) => eq_nocase(data, s, nocase),
            Pred::Match(s) => contains(data, s, nocase),
            Pred::Glob(s) => glob_match(s, data, nocase),
            Pred::Re(_, r) => r.as_ref().is_some_and(|r| r.is_match(data)),
        };
        if and { preds.iter().all(one) } else { preds.iter().any(one) }
    };
    let mut out = Vec::new();
    for (idx, v) in a.scan(resolve(start), resolve(end)) {
        if out.len() as u64 >= limit {
            break;
        }
        if matches(&v.text()) {
            out.push(if withvalues { Reply::Array(vec![unsigned(idx), value(Some(v))]) } else { unsigned(idx) });
        }
    }
    Ok(Reply::Array(out))
}

// ---- AROP ----

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Sum,
    Min,
    Max,
    And,
    Or,
    Xor,
    Match,
    Used,
}

/// A value as AROP's numeric operations see it: (number, integer value if integral).
fn numeric(v: &Val) -> Option<(Ld, Option<i64>)> {
    match v.decoded() {
        Decoded::Int(i) => Some((Ld::from_i64(i), Some(i))),
        Decoded::Float(d) => Some((Ld::from_f64(d), None)),
        Decoded::Small(s) | Decoded::Heap(s) => match parse_i64(s) {
            Some(i) => Some((Ld::from_i64(i), Some(i))),
            None => Ld::parse(s).map(|l| (l, None)),
        },
    }
}

/// AROP key start end SUM|MIN|MAX|AND|OR|XOR|USED | MATCH value
pub fn arop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (start, end) = (index(&args[2])?, index(&args[3])?);
    let op = [("sum", Op::Sum), ("min", Op::Min), ("max", Op::Max), ("and", Op::And), ("or", Op::Or), ("xor", Op::Xor), ("match", Op::Match), ("used", Op::Used)]
        .into_iter()
        .find(|(k, _)| kw(&args[4], k))
        .map(|(_, o)| o)
        .ok_or(Error::new("ERR unknown operation"))?;
    if op == Op::Match {
        if args.len() != 6 {
            return Err(Error::new("ERR MATCH requires a value argument"));
        }
    } else if args.len() != 5 {
        return Err(arity_error("arop"));
    }
    let Some(a) = cx.get_array(&args[1])? else {
        return Ok(if matches!(op, Op::Match | Op::Used) { Reply::Int(0) } else { Reply::Nil });
    };
    let items = a.scan(start, end).map(|(_, v)| v);
    Ok(match op {
        Op::Used => Reply::Int(items.count() as i64),
        Op::Match => Reply::Int(items.filter(|v| v.text().as_ref() == args[5].as_ref()).count() as i64),
        Op::And | Op::Or | Op::Xor => {
            let mut acc: Option<i64> = None;
            for v in items {
                let Some((num, int)) = numeric(v) else { continue };
                // Floats count by their integer part, when it fits.
                let Some(i) = int.or_else(|| num.to_i64()) else { continue };
                acc = Some(match (acc, op) {
                    (None, _) => i,
                    (Some(a), Op::And) => a & i,
                    (Some(a), Op::Or) => a | i,
                    (Some(a), _) => a ^ i,
                });
            }
            acc.map_or(Reply::Nil, Reply::Int)
        }
        _ => {
            let mut acc: Option<Ld> = None;
            for v in items {
                let Some((num, _)) = numeric(v) else { continue };
                acc = Some(match acc {
                    None => num,
                    Some(a) if op == Op::Sum => a.add(&num),
                    Some(a) if op == Op::Min && num.compare(&a) == std::cmp::Ordering::Less && !num.is_nan() && !a.is_nan() => num,
                    Some(a) if op == Op::Max && num.compare(&a) == std::cmp::Ordering::Greater && !num.is_nan() && !a.is_nan() => num,
                    Some(a) => a,
                });
            }
            acc.map_or(Reply::Nil, |a| Reply::bulk_str(a.auto()))
        }
    })
}

// ---- ring buffers ----

/// ARINSERT key value [value ...]: append at the insert cursor; returns the last index written.
pub fn arinsert(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let lim = limits(cx);
    let mut e = write_array(cx, &args[1])?;
    let a = e.array();
    let n = (args.len() - 2) as u64;
    let start = match a.insert_idx() {
        array::INSERT_NONE => 0,
        i if i >= u64::MAX - 1 => {
            e.unchanged();
            return Err(INSERT_OVERFLOW);
        }
        i => i + 1,
    };
    let Some(last) = start.checked_add(n - 1).filter(|&l| l != u64::MAX) else {
        e.unchanged();
        return Err(INSERT_OVERFLOW);
    };
    if n > 1 {
        a.promote_for_range_set(start, last, lim);
    }
    for (i, v) in args[2..].iter().enumerate() {
        a.set(start + i as u64, Val::encode(v), lim);
    }
    a.set_insert_idx(last);
    e.notify(notify::ARRAY, "arinsert");
    Ok(unsigned(last))
}

/// `arRingRework`: keep up to `keep` positions walking back from the cursor (stopping at the
/// first hole), replayed oldest first into a fresh array.
fn ring_rework(a: &Array, old_span: u64, keep: u64, slice_size: u32, lim: Limits) -> Array {
    let mut out = Array::new(slice_size);
    let anchor = if a.insert_idx() == array::INSERT_NONE { old_span - 1 } else { a.insert_idx() % old_span };
    let mut retained = 0;
    let mut src = anchor;
    while retained < keep {
        if a.get(src).is_none() {
            break;
        }
        retained += 1;
        src = if src == 0 { old_span - 1 } else { src - 1 };
    }
    src += 1;
    if src == old_span {
        src = 0;
    }
    for dst in 0..retained {
        out.set(dst, a.get(src).expect("retained").clone(), lim);
        src += 1;
        if src == old_span {
            src = 0;
        }
    }
    if retained != 0 {
        out.set_insert_idx(retained - 1);
    }
    out
}

/// ARRING key size value [value ...]: write into a ring of `size` slots; returns the last slot.
pub fn arring(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let size = parse_i64(&args[2]).ok_or(Error::new("ERR invalid size"))?;
    if size <= 0 {
        return Err(Error::new("ERR size must be positive"));
    }
    let size = size as u64;
    let lim = limits(cx);
    let slice_size = cx.cfg().array_slice_size;
    let mut e = write_array(cx, &args[1])?;
    let a = e.array();
    let old_span = a.len();
    let keep = if old_span == 0 || size == old_span {
        0
    } else if size < old_span {
        size
    } else if a.insert_idx() != array::INSERT_NONE && a.insert_idx() + 1 < old_span {
        old_span
    } else {
        0
    };
    if keep > 0 {
        *a = ring_rework(a, old_span, keep, slice_size, lim);
    }
    let mut cursor = 0;
    for v in &args[3..] {
        cursor = match a.insert_idx() {
            array::INSERT_NONE => 0,
            i => i.wrapping_add(1),
        };
        if cursor >= size {
            cursor %= size;
        }
        a.set(cursor, Val::encode(v), lim);
        a.set_insert_idx(cursor);
    }
    e.notify(notify::ARRAY, "arring");
    Ok(unsigned(cursor))
}

/// ARNEXT key: the index the next ARINSERT would use (null when exhausted).
pub fn arnext(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let Some(a) = cx.get_array(&args[1])? else { return Ok(Reply::Int(0)) };
    Ok(match a.insert_idx() {
        array::INSERT_NONE => Reply::Int(0),
        i if i == u64::MAX - 1 => Reply::Nil,
        i => unsigned(i + 1),
    })
}

/// ARSEEK key index: set the next insert index (0 resets the cursor).
pub fn arseek(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let idx = parse_index(&args[2], true).ok_or(INVALID_INDEX)?;
    let Some(mut e) = cx.get_mut_typed(&args[1], Kind::Array)? else { return Ok(Reply::Int(0)) };
    e.array().set_insert_idx(if idx == 0 { array::INSERT_NONE } else { idx - 1 });
    e.notify(notify::ARRAY, "arseek");
    Ok(Reply::Int(1))
}

/// ARLASTITEMS key count [REV]: the latest positions walking back from the cursor.
pub fn arlastitems(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let count = parse_i64(&args[2]).ok_or(Error::new("ERR invalid COUNT"))?;
    if count <= 0 {
        return Ok(Reply::Array(vec![]));
    }
    let rev = match args.len() {
        4 if kw(&args[3], "rev") => true,
        4 => return Err(SYNTAX),
        3 => false,
        _ => return Err(arity_error("arlastitems")),
    };
    let Some(a) = cx.get_array(&args[1])? else { return Ok(Reply::Array(vec![])) };
    let len = a.len();
    let n = (count as u64).min(a.count());
    if n == 0 {
        return Ok(Reply::Array(vec![]));
    }
    let mut cur = if a.insert_idx() == array::INSERT_NONE { len - 1 } else { a.insert_idx() };
    let mut items = Vec::with_capacity(n as usize);
    for _ in 0..n {
        items.push(value(a.get(cur)));
        cur = if cur == 0 { len - 1 } else { cur - 1 };
    }
    if !rev {
        items.reverse();
    }
    Ok(Reply::Array(items))
}

/// ARINFO key [FULL]
pub fn arinfo(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let full = match args.len() {
        2 => false,
        3 if kw(&args[2], "full") => true,
        _ => return Err(SYNTAX),
    };
    let a = cx.get_array(&args[1])?.ok_or(NO_KEY)?;
    let i = a.info(full);
    let f = |k: &str, v: Reply| (Reply::bulk(k), v);
    let next = match i.insert_idx {
        array::INSERT_NONE => Reply::Int(0),
        x if x == u64::MAX - 1 => Reply::Int(0),
        x => unsigned(x + 1),
    };
    let mut m = vec![
        f("count", unsigned(i.count)),
        f("len", unsigned(i.len)),
        f("next-insert-index", next),
        f("slices", Reply::Int(i.slices as i64)),
        f("directory-size", Reply::Int(i.directory_size as i64)),
        f("super-dir-entries", Reply::Int(i.super_dir_entries as i64)),
        f("slice-size", Reply::Int(i.slice_size as i64)),
    ];
    if full {
        let avg = |a: u64, b: u64| if b > 0 { a as f64 / b as f64 } else { 0.0 };
        m.push(f("dense-slices", Reply::Int(i.dense_slices as i64)));
        m.push(f("sparse-slices", Reply::Int(i.sparse_slices as i64)));
        m.push(f("avg-dense-size", Reply::Double(avg(i.dense_total_winsize, i.dense_slices))));
        m.push(f("avg-dense-fill", Reply::Double(avg(i.dense_total_count, i.dense_total_winsize))));
        m.push(f("avg-sparse-size", Reply::Double(avg(i.sparse_total_cap, i.sparse_slices))));
    }
    Ok(Reply::Map(m))
}
