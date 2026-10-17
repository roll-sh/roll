//! Sorted set commands (`t_zset.c`).

use super::keys::{parse_cursor, scan_reply};
use super::*;
use crate::engine::{Engine, Mode};
use crate::notify;
use crate::types::zset::{AddFlags, AddOut, LexBound, LexRange, Limits, ScoreRange};
use crate::types::{Set, ZSet};
use crate::util::{Rng, glob_match, parse_i64};
use crate::value::{Kind, Str, Value};
use crate::{cmd, util};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

pub static COMMANDS: &[Command] = &[
    cmd!("zadd", -4, WRITE | DENYOOM | FAST, 1, 1, 1, zadd, SortedSet),
    cmd!("zincrby", 4, WRITE | DENYOOM | FAST, 1, 1, 1, zincrby, SortedSet),
    cmd!("zrem", -3, WRITE | FAST, 1, 1, 1, zrem, SortedSet),
    cmd!("zremrangebyscore", 4, WRITE, 1, 1, 1, zremrangebyscore, SortedSet),
    cmd!("zremrangebyrank", 4, WRITE, 1, 1, 1, zremrangebyrank, SortedSet),
    cmd!("zremrangebylex", 4, WRITE, 1, 1, 1, zremrangebylex, SortedSet),
    cmd!("zunionstore", -4, WRITE | DENYOOM, 1, 1, 1, keys = store_op_keys, zunionstore, SortedSet),
    cmd!("zinterstore", -4, WRITE | DENYOOM, 1, 1, 1, keys = store_op_keys, zinterstore, SortedSet),
    cmd!("zdiffstore", -4, WRITE | DENYOOM, 1, 1, 1, keys = store_op_keys, zdiffstore, SortedSet),
    cmd!("zunion", -3, READONLY, keys = op_keys, zunion, SortedSet),
    cmd!("zinter", -3, READONLY, keys = op_keys, zinter, SortedSet),
    cmd!("zintercard", -3, READONLY, keys = op_keys, zintercard, SortedSet),
    cmd!("zdiff", -3, READONLY, keys = op_keys, zdiff, SortedSet),
    cmd!("zrange", -4, READONLY, 1, 1, 1, zrange, SortedSet),
    cmd!("zrangestore", -5, WRITE | DENYOOM, 1, 2, 1, zrangestore, SortedSet),
    cmd!("zrangebyscore", -4, READONLY, 1, 1, 1, zrangebyscore, SortedSet),
    cmd!("zrevrangebyscore", -4, READONLY, 1, 1, 1, zrevrangebyscore, SortedSet),
    cmd!("zrangebylex", -4, READONLY, 1, 1, 1, zrangebylex, SortedSet),
    cmd!("zrevrangebylex", -4, READONLY, 1, 1, 1, zrevrangebylex, SortedSet),
    cmd!("zcount", 4, READONLY | FAST, 1, 1, 1, zcount, SortedSet),
    cmd!("zlexcount", 4, READONLY | FAST, 1, 1, 1, zlexcount, SortedSet),
    cmd!("zrevrange", -4, READONLY, 1, 1, 1, zrevrange, SortedSet),
    cmd!("zcard", 2, READONLY | FAST, 1, 1, 1, zcard, SortedSet),
    cmd!("zscore", 3, READONLY | FAST, 1, 1, 1, zscore, SortedSet),
    cmd!("zmscore", -3, READONLY | FAST, 1, 1, 1, zmscore, SortedSet),
    cmd!("zrank", -3, READONLY | FAST, 1, 1, 1, zrank, SortedSet),
    cmd!("zrevrank", -3, READONLY | FAST, 1, 1, 1, zrevrank, SortedSet),
    cmd!("zscan", -3, READONLY, 1, 1, 1, zscan, SortedSet),
    cmd!("zpopmin", -2, WRITE | FAST, 1, 1, 1, zpopmin, SortedSet),
    cmd!("zpopmax", -2, WRITE | FAST, 1, 1, 1, zpopmax, SortedSet),
    cmd!("zmpop", -4, WRITE, keys = zmpop_keys, zmpop, SortedSet),
    cmd!("bzpopmin", -3, WRITE | BLOCKING | FAST, 1, -2, 1, bzpopmin, SortedSet),
    cmd!("bzpopmax", -3, WRITE | BLOCKING | FAST, 1, -2, 1, bzpopmax, SortedSet),
    cmd!("bzmpop", -5, WRITE | BLOCKING, keys = bzmpop_keys, bzmpop, SortedSet),
    cmd!("zrandmember", -2, READONLY, 1, 1, 1, zrandmember, SortedSet),
];

const NAN_ERR: Error = Error::new("ERR resulting score is not a number (NaN)");
const RANGE_NOT_FLOAT: Error = Error::new("ERR min or max is not a float");
const LEX_INVALID: Error = Error::new("ERR min or max not valid string range item");

fn b(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

pub(crate) fn limits(cx: &Cx) -> Limits {
    Limits::from_cfg(cx.cfg())
}

// ---- key positions ----

/// `numkeys` at `idx`, keys right after it.
fn numkeys_at(args: &[Bytes], idx: usize) -> Vec<usize> {
    let Some(n) = args.get(idx).and_then(|a| parse_i64(a)) else { return vec![] };
    let n = (n.max(0) as usize).min(args.len());
    (idx + 1..idx + 1 + n).filter(|&i| i < args.len()).collect()
}

fn op_keys(args: &[Bytes]) -> Vec<usize> {
    numkeys_at(args, 1)
}

fn store_op_keys(args: &[Bytes]) -> Vec<usize> {
    let mut v = vec![1];
    v.extend(numkeys_at(args, 2));
    v
}

fn zmpop_keys(args: &[Bytes]) -> Vec<usize> {
    numkeys_at(args, 1)
}

fn bzmpop_keys(args: &[Bytes]) -> Vec<usize> {
    numkeys_at(args, 2)
}

// ---- argument parsing ----

/// `getDoubleFromObject` (`string2d`): a double with no surrounding spaces, where overflow to
/// infinity and underflow to zero are errors (unlike the `inf` spellings).
pub(crate) fn zfloat(a: &[u8]) -> Option<f64> {
    let v = redis_proto::parse_double(a)?;
    let body = a.strip_prefix(b"+").or_else(|| a.strip_prefix(b"-")).unwrap_or(a);
    if v.is_infinite() {
        if !(eq(body, "inf") || eq(body, "infinity")) {
            return None;
        }
    } else if v == 0.0 {
        // Underflow (ERANGE with a zero result).
        let nonzero = if body.len() > 2 && (body[..2].eq_ignore_ascii_case(b"0x")) {
            body[2..].iter().take_while(|c| !matches!(c, b'p' | b'P')).any(|c| c.is_ascii_hexdigit() && *c != b'0')
        } else {
            body.iter().take_while(|c| !matches!(c, b'e' | b'E')).any(|c| (b'1'..=b'9').contains(c))
        };
        if nonzero {
            return None;
        }
    }
    Some(v)
}

/// `fast_float_strtod` as used for score ranges: leading whitespace allowed, the empty string is
/// 0, the whole input must be consumed, NaN rejected.
fn strtod_full(s: &[u8]) -> Option<f64> {
    if s.is_empty() {
        return Some(0.0);
    }
    let start = s.iter().position(|c| !matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))?;
    redis_proto::parse_double(&s[start..])
}

/// `zslParseRange`.
pub(crate) fn parse_score_range(min: &[u8], max: &[u8]) -> Result<ScoreRange, Error> {
    let one = |a: &[u8]| -> Option<(f64, bool)> {
        match a.first() {
            Some(b'(') => Some((strtod_full(&a[1..])?, true)),
            _ => Some((strtod_full(a)?, false)),
        }
    };
    let (min, minex) = one(min).ok_or(RANGE_NOT_FLOAT)?;
    let (max, maxex) = one(max).ok_or(RANGE_NOT_FLOAT)?;
    Ok(ScoreRange { min, max, minex, maxex })
}

fn parse_lex_item(a: &[u8]) -> Option<(LexBound, bool)> {
    match *a.first()? {
        b'+' if a.len() == 1 => Some((LexBound::Max, true)),
        b'-' if a.len() == 1 => Some((LexBound::Min, true)),
        b'(' => Some((LexBound::Str(a[1..].to_vec()), true)),
        b'[' => Some((LexBound::Str(a[1..].to_vec()), false)),
        _ => None,
    }
}

/// `zslParseLexRange`.
fn parse_lex_range(min: &[u8], max: &[u8]) -> Result<LexRange, Error> {
    let (min, minex) = parse_lex_item(min).ok_or(LEX_INVALID)?;
    let (max, maxex) = parse_lex_item(max).ok_or(LEX_INVALID)?;
    Ok(LexRange { min, max, minex, maxex })
}

/// `getPositiveLongFromObjectOrReply` with a message used for both parse and range errors.
fn positive(a: &[u8], msg: &'static str) -> Result<i64, Error> {
    match parse_i64(a) {
        Some(v) if v >= 0 => Ok(v),
        _ => Err(Error::new(msg)),
    }
}

// ---- reply helpers ----

/// Push a member (and its score): flat pairs in RESP2, `[member, score]` arrays in RESP3.
fn push_scored(out: &mut Vec<Reply>, resp3: bool, withscores: bool, m: &[u8], s: f64) {
    if !withscores {
        out.push(Reply::bulk(m));
    } else if resp3 {
        out.push(Reply::Array(vec![Reply::bulk(m), Reply::Double(s)]));
    } else {
        out.push(Reply::bulk(m));
        out.push(Reply::Double(s));
    }
}

/// Read-only access to a zset without going through the `Cx` lookup again (call after a
/// `cx.get*` did expiry, stats and type checks).
pub(crate) fn zset_ref<'b>(cx: &'b Cx<'_>, key: &[u8]) -> Option<&'b ZSet> {
    let now = cx.eng.expiry_clock(cx.now);
    match cx.eng.dbs[cx.client.db].keys.get(key) {
        Some(e) if !e.expired(now) => match &e.value {
            Value::ZSet(z) => Some(z),
            _ => None,
        },
        _ => None,
    }
}

/// Write `z` into `dst` (or delete `dst` when empty), with the STORE notifications. Returns the
/// stored length.
pub(crate) fn store_result(cx: &mut Cx, dst: &[u8], z: ZSet, event: &'static str) -> i64 {
    let n = z.len();
    if n == 0 {
        if cx.delete(dst) {
            cx.notify(notify::GENERIC, "del", dst);
        }
        return 0;
    }
    cx.set(dst, Value::ZSet(Box::new(z)), false);
    cx.notify(notify::ZSET, event, dst);
    n as i64
}

/// Build a zset from (member, score) pairs in ascending order.
pub(crate) fn zset_from_pairs<M: AsRef<[u8]>>(items: &[(M, f64)], lim: Limits) -> ZSet {
    ZSet::from_sorted(items.iter().map(|(m, s)| (m.as_ref(), *s)), lim)
}

fn sort_pairs<M: AsRef<[u8]>>(v: &mut [(M, f64)]) {
    v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.as_ref().cmp(b.0.as_ref())));
}

// ---- ZADD / ZINCRBY ----

pub fn zadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zadd_generic(cx, args, false)
}

pub fn zincrby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zadd_generic(cx, args, true)
}

/// `zaddGenericCommand` (also used by GEOADD with a rewritten argument vector).
pub(crate) fn zadd_generic(cx: &mut Cx, args: &[Bytes], incr: bool) -> CmdResult {
    let mut f = AddFlags { incr, ..Default::default() };
    let mut ch = false;
    let mut i = 2;
    while i < args.len() {
        let a = &args[i];
        if eq(a, "nx") {
            f.nx = true;
        } else if eq(a, "xx") {
            f.xx = true;
        } else if eq(a, "ch") {
            ch = true;
        } else if eq(a, "incr") {
            f.incr = true;
        } else if eq(a, "gt") {
            f.gt = true;
        } else if eq(a, "lt") {
            f.lt = true;
        } else {
            break;
        }
        i += 1;
    }
    let n = args.len() - i;
    if !n.is_multiple_of(2) || n == 0 {
        return Err(SYNTAX);
    }
    let elements = n / 2;
    if f.nx && f.xx {
        return Err(Error::new("ERR XX and NX options at the same time are not compatible"));
    }
    if (f.gt && f.nx) || (f.lt && f.nx) || (f.gt && f.lt) {
        return Err(Error::new("ERR GT, LT, and/or NX options at the same time are not compatible"));
    }
    if f.incr && elements > 1 {
        return Err(Error::new("ERR INCR option supports a single increment-element pair"));
    }
    let mut scores = Vec::with_capacity(elements);
    for j in 0..elements {
        scores.push(zfloat(&args[i + 2 * j]).ok_or(NOT_FLOAT)?);
    }
    let key = &args[1];
    let lim = limits(cx);
    let exists = cx.check_type(key, Kind::ZSet)?;
    if !exists && f.xx {
        return Ok(if f.incr { Reply::Nil } else { Reply::Int(0) });
    }
    let mut e = cx.get_or_create(key, Kind::ZSet)?;
    let z = e.zset();
    if !exists {
        *z = ZSet::with_hint(elements, args[i + 1].len(), lim);
    } else {
        z.maybe_convert(elements, lim);
    }
    let (mut added, mut updated, mut processed) = (0i64, 0i64, 0i64);
    let mut score = 0.0;
    let mut nan = false;
    for (j, &s) in scores.iter().enumerate() {
        match z.add(&args[i + 1 + 2 * j], s, f, lim) {
            Err(_) => {
                nan = true;
                break;
            }
            Ok((out, ns)) => {
                match out {
                    AddOut::Added => added += 1,
                    AddOut::Updated => updated += 1,
                    _ => {}
                }
                if out != AddOut::Nop {
                    processed += 1;
                }
                score = ns;
            }
        }
    }
    if added + updated > 0 {
        e.notify(notify::ZSET, if f.incr { "zincr" } else { "zadd" });
        e.changes((added + updated) as usize);
    } else {
        e.unchanged();
    }
    drop(e);
    if nan {
        return Err(NAN_ERR);
    }
    Ok(if f.incr {
        if processed > 0 { Reply::Double(score) } else { Reply::Nil }
    } else {
        Reply::Int(if ch { added + updated } else { added })
    })
}

// ---- ZREM / ZREMRANGEBY* ----

pub fn zrem(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let Some(mut e) = cx.get_mut_typed(&args[1], Kind::ZSet)? else { return Ok(Reply::Int(0)) };
    let z = e.zset();
    let mut deleted = 0;
    for m in &args[2..] {
        if z.remove(m) {
            deleted += 1;
        }
        if z.is_empty() {
            break;
        }
    }
    if deleted > 0 {
        e.notify(notify::ZSET, "zrem");
        e.changes(deleted as usize);
    } else {
        e.unchanged();
    }
    Ok(Reply::Int(deleted))
}

enum RemRange {
    Rank(i64, i64),
    Score(ScoreRange),
    Lex(LexRange),
}

fn zremrange(cx: &mut Cx, args: &[Bytes], r: RemRange, event: &'static str) -> CmdResult {
    let Some(mut e) = cx.get_mut_typed(&args[1], Kind::ZSet)? else { return Ok(Reply::Int(0)) };
    let z = e.zset();
    let deleted = match &r {
        RemRange::Rank(s, t) => match range(*s, *t, z.len()) {
            Some((a, b)) => z.remove_ranks(a, b),
            None => 0,
        },
        RemRange::Score(r) => z.remove_in(r.nonempty(), |s, _| r.gte_min(s), |s, _| r.lte_max(s)),
        RemRange::Lex(r) => z.remove_in(r.nonempty(), |_, m| r.gte_min(m), |_, m| r.lte_max(m)),
    };
    if deleted > 0 {
        e.notify(notify::ZSET, event);
        e.changes(deleted);
    } else {
        e.unchanged();
    }
    Ok(Reply::Int(deleted as i64))
}

pub fn zremrangebyrank(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let r = RemRange::Rank(int(&args[2])?, int(&args[3])?);
    zremrange(cx, args, r, "zremrangebyrank")
}

pub fn zremrangebyscore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let r = RemRange::Score(parse_score_range(&args[2], &args[3])?);
    zremrange(cx, args, r, "zremrangebyscore")
}

pub fn zremrangebylex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let r = RemRange::Lex(parse_lex_range(&args[2], &args[3])?);
    zremrange(cx, args, r, "zremrangebylex")
}

// ---- ZUNION / ZINTER / ZDIFF (+STORE), ZINTERCARD ----

#[derive(Clone, Copy, PartialEq)]
enum SetOp {
    Union,
    Inter,
    Diff,
}

#[derive(Clone, Copy, PartialEq)]
enum Agg {
    Sum,
    Min,
    Max,
    Count,
}

#[derive(Clone, Copy)]
enum Src<'a> {
    Missing,
    Z(&'a ZSet),
    S(&'a Set),
}

impl<'a> Src<'a> {
    fn len(&self) -> usize {
        match self {
            Src::Missing => 0,
            Src::Z(z) => z.len(),
            Src::S(s) => s.len(),
        }
    }

    fn find(&self, m: &[u8]) -> Option<f64> {
        match self {
            Src::Missing => None,
            Src::Z(z) => z.score(m),
            Src::S(s) => s.contains(m).then_some(1.0),
        }
    }

    fn iter(&self) -> Box<dyn Iterator<Item = (Cow<'a, [u8]>, f64)> + 'a> {
        match *self {
            Src::Missing => Box::new(std::iter::empty()),
            Src::Z(z) => Box::new(z.iter().map(|(m, s)| (Cow::Borrowed(m), s))),
            Src::S(s) => Box::new(s.iter_members().map(|m| (m, 1.0))),
        }
    }
}

#[inline]
fn weighted(score: f64, weight: f64, agg: Agg) -> f64 {
    if agg == Agg::Count { weight } else { weight * score }
}

#[inline]
fn aggregate(target: &mut f64, val: f64, agg: Agg) {
    match agg {
        Agg::Sum | Agg::Count => {
            *target += val;
            if target.is_nan() {
                *target = 0.0;
            }
        }
        Agg::Min => {
            if val < *target {
                *target = val;
            }
        }
        Agg::Max => {
            if val > *target {
                *target = val;
            }
        }
    }
}

/// `zunionInterDiffGenericCommand`.
fn zsetop(cx: &mut Cx, args: &[Bytes], dst: Option<&Bytes>, numkeys_idx: usize, op: SetOp, card_only: bool) -> CmdResult {
    let n = int(&args[numkeys_idx])?;
    if n < 1 {
        return Err(Error::msg(format!("ERR at least 1 input key is needed for '{}' command", cx.cmd.name)));
    }
    if n as u64 > (args.len() - numkeys_idx - 1) as u64 {
        return Err(SYNTAX);
    }
    let n = n as usize;
    let keys = &args[numkeys_idx + 1..numkeys_idx + 1 + n];
    for k in keys {
        match cx.get(k) {
            None | Some(Value::ZSet(_)) | Some(Value::Set(_)) => {}
            Some(_) => return Err(WRONGTYPE),
        }
    }
    let mut weights = vec![1.0f64; n];
    let mut agg = Agg::Sum;
    let mut withscores = false;
    let mut limit = 0usize;
    let mut j = numkeys_idx + 1 + n;
    while j < args.len() {
        let remaining = args.len() - j;
        let a = &args[j];
        if op != SetOp::Diff && !card_only && remaining > n && eq(a, "weights") {
            j += 1;
            for w in weights.iter_mut() {
                *w = zfloat(&args[j]).ok_or(Error::new("ERR weight value is not a float"))?;
                j += 1;
            }
        } else if op != SetOp::Diff && !card_only && remaining >= 2 && eq(a, "aggregate") {
            let v = &args[j + 1];
            agg = if eq(v, "sum") {
                Agg::Sum
            } else if eq(v, "min") {
                Agg::Min
            } else if eq(v, "max") {
                Agg::Max
            } else if eq(v, "count") {
                Agg::Count
            } else {
                return Err(SYNTAX);
            };
            j += 2;
        } else if dst.is_none() && !card_only && eq(a, "withscores") {
            withscores = true;
            j += 1;
        } else if card_only && remaining >= 2 && eq(a, "limit") {
            limit = positive(&args[j + 1], "ERR LIMIT can't be negative")? as usize;
            j += 2;
        } else {
            return Err(SYNTAX);
        }
    }

    let lim = limits(cx);
    let resp3 = cx.resp() >= 3;
    let now = cx.eng.expiry_clock(cx.now);
    let db = &cx.eng.dbs[cx.client.db];
    let mut srcs: Vec<(Src, f64, &Bytes)> = keys
        .iter()
        .zip(weights)
        .map(|(k, w)| {
            let src = match db.keys.get(k) {
                Some(e) if !e.expired(now) => match &e.value {
                    Value::ZSet(z) => Src::Z(z),
                    Value::Set(s) => Src::S(s),
                    _ => Src::Missing,
                },
                _ => Src::Missing,
            };
            (src, w, k)
        })
        .collect();
    if op != SetOp::Diff {
        refsort::qsort(&mut srcs, |a, b| a.0.len().cmp(&b.0.len()));
    }

    let mut out: Vec<(Cow<[u8]>, f64)> = Vec::new();
    let mut card = 0usize;
    match op {
        SetOp::Inter => {
            if srcs[0].0.len() > 0 {
                for (m, s0) in srcs[0].0.iter() {
                    let mut score = weighted(s0, srcs[0].1, agg);
                    if score.is_nan() {
                        score = 0.0;
                    }
                    let mut all = true;
                    for (src, w, _) in &srcs[1..] {
                        match src.find(&m) {
                            Some(v) => aggregate(&mut score, weighted(v, *w, agg), agg),
                            None => {
                                all = false;
                                break;
                            }
                        }
                    }
                    if !all {
                        continue;
                    }
                    if card_only {
                        card += 1;
                        if limit > 0 && card >= limit {
                            break;
                        }
                    } else {
                        out.push((m, score));
                    }
                }
            }
        }
        SetOp::Union => {
            let mut acc: HashMap<Cow<[u8]>, f64> = HashMap::with_capacity(srcs.last().map_or(0, |s| s.0.len()));
            for (src, w, _) in &srcs {
                for (m, s) in src.iter() {
                    let mut score = weighted(s, *w, agg);
                    if score.is_nan() {
                        score = 0.0;
                    }
                    match acc.get_mut(m.as_ref()) {
                        Some(t) => aggregate(t, score, agg),
                        None => {
                            acc.insert(m, score);
                        }
                    }
                }
            }
            out = acc.into_iter().collect();
        }
        SetOp::Diff => {
            let first = &srcs[0];
            let same = srcs[1..].iter().any(|(s, _, k)| !matches!(s, Src::Missing) && *k == first.2);
            if first.0.len() > 0 && !same {
                for (m, s) in first.0.iter() {
                    if srcs[1..].iter().all(|(src, _, _)| src.find(&m).is_none()) {
                        out.push((m, s));
                    }
                }
            }
        }
    }
    if card_only {
        return Ok(Reply::Int(card as i64));
    }
    sort_pairs(&mut out);
    match dst {
        Some(dst) => {
            let z = zset_from_pairs(&out, lim);
            drop(out);
            let event = match op {
                SetOp::Union => "zunionstore",
                SetOp::Inter => "zinterstore",
                SetOp::Diff => "zdiffstore",
            };
            Ok(Reply::Int(store_result(cx, dst, z, event)))
        }
        None => {
            let mut r = Vec::with_capacity(out.len() * if withscores && !resp3 { 2 } else { 1 });
            for (m, s) in &out {
                push_scored(&mut r, resp3, withscores, m, *s);
            }
            Ok(Reply::Array(r))
        }
    }
}

pub fn zunionstore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zsetop(cx, args, Some(&args[1]), 2, SetOp::Union, false)
}
pub fn zinterstore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zsetop(cx, args, Some(&args[1]), 2, SetOp::Inter, false)
}
pub fn zdiffstore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zsetop(cx, args, Some(&args[1]), 2, SetOp::Diff, false)
}
pub fn zunion(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zsetop(cx, args, None, 1, SetOp::Union, false)
}
pub fn zinter(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zsetop(cx, args, None, 1, SetOp::Inter, false)
}
pub fn zintercard(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zsetop(cx, args, None, 1, SetOp::Inter, true)
}
pub fn zdiff(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zsetop(cx, args, None, 1, SetOp::Diff, false)
}

// ---- ZRANGE family ----

#[derive(Clone, Copy, PartialEq)]
enum RangeType {
    Auto,
    Rank,
    Score,
    Lex,
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Auto,
    Fwd,
    Rev,
}

enum Spec {
    Rank(i64, i64),
    Score(ScoreRange),
    Lex(LexRange),
}

/// Walk the elements selected by `spec` in emission order.
fn walk(z: &ZSet, spec: &Spec, rev: bool, offset: i64, limit: i64, f: &mut dyn FnMut(&[u8], f64)) {
    match spec {
        Spec::Rank(s, e) => {
            let len = z.len();
            let Some((a, b)) = range(*s, *e, len) else { return };
            let from = if rev { len - 1 - a } else { a };
            for (m, s) in z.iter_from(from, rev).take(b - a + 1) {
                f(m, s);
            }
        }
        Spec::Score(r) => walk_in(z, rev, offset, limit, r.nonempty(), |s, _| r.gte_min(s), |s, _| r.lte_max(s), f),
        Spec::Lex(r) => walk_in(z, rev, offset, limit, r.nonempty(), |_, m| r.gte_min(m), |_, m| r.lte_max(m), f),
    }
}

/// `genericZrangebyscoreCommand` / `genericZrangebylexCommand`.
#[allow(clippy::too_many_arguments)]
fn walk_in(
    z: &ZSet,
    rev: bool,
    offset: i64,
    mut limit: i64,
    nonempty: bool,
    gte: impl Fn(f64, &[u8]) -> bool,
    lte: impl Fn(f64, &[u8]) -> bool,
    f: &mut dyn FnMut(&[u8], f64),
) {
    let len = z.len();
    if offset < 0 || (offset > 0 && offset as u64 >= len as u64) {
        return;
    }
    let offset = offset as usize;
    if !rev {
        let Some(first) = z.first_in(nonempty, &gte, &lte) else { return };
        for (m, s) in z.iter_from(first + offset, false) {
            if limit == 0 || !lte(s, m) {
                break;
            }
            limit -= 1;
            f(m, s);
        }
    } else {
        let Some(last) = z.last_in(nonempty, &gte, &lte) else { return };
        if offset > last {
            return;
        }
        for (m, s) in z.iter_from(last - offset, true) {
            if limit == 0 || !gte(s, m) {
                break;
            }
            limit -= 1;
            f(m, s);
        }
    }
}

/// `zrangeGenericCommand`: `args[start]` is the source key.
fn zrange_generic(cx: &mut Cx, args: &[Bytes], start: usize, dst: Option<&Bytes>, mut kind: RangeType, mut dir: Dir) -> CmdResult {
    let key = &args[start];
    let (mut minidx, mut maxidx) = (start + 1, start + 2);
    let mut withscores = false;
    let (mut offset, mut limit) = (0i64, -1i64);
    let mut j = start + 3;
    while j < args.len() {
        let a = &args[j];
        let left = args.len() - j - 1;
        if dst.is_none() && eq(a, "withscores") {
            withscores = true;
        } else if eq(a, "limit") && left >= 2 {
            offset = int(&args[j + 1])?;
            limit = int(&args[j + 2])?;
            j += 2;
        } else if dir == Dir::Auto && eq(a, "rev") {
            dir = Dir::Rev;
        } else if kind == RangeType::Auto && eq(a, "bylex") {
            kind = RangeType::Lex;
        } else if kind == RangeType::Auto && eq(a, "byscore") {
            kind = RangeType::Score;
        } else {
            return Err(SYNTAX);
        }
        j += 1;
    }
    if dir == Dir::Auto {
        dir = Dir::Fwd;
    }
    if kind == RangeType::Auto {
        kind = RangeType::Rank;
    }
    if limit != -1 && kind == RangeType::Rank {
        return Err(Error::new("ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX"));
    }
    if withscores && kind == RangeType::Lex {
        return Err(Error::new("ERR syntax error, WITHSCORES not supported in combination with BYLEX"));
    }
    let rev = dir == Dir::Rev;
    if rev && kind != RangeType::Rank {
        std::mem::swap(&mut minidx, &mut maxidx);
    }
    let spec = match kind {
        RangeType::Score => Spec::Score(parse_score_range(&args[minidx], &args[maxidx])?),
        RangeType::Lex => Spec::Lex(parse_lex_range(&args[minidx], &args[maxidx])?),
        _ => Spec::Rank(int(&args[minidx])?, int(&args[maxidx])?),
    };
    let resp3 = cx.resp() >= 3;
    match dst {
        None => {
            let Some(z) = cx.get_zset(key)? else { return Ok(Reply::Array(vec![])) };
            let mut out = Vec::new();
            walk(z, &spec, rev, offset, limit, &mut |m, s| push_scored(&mut out, resp3, withscores, m, s));
            Ok(Reply::Array(out))
        }
        Some(dst) => {
            let lim = limits(cx);
            let mut items: Vec<(Str, f64)> = Vec::new();
            if let Some(z) = cx.get_zset(key)? {
                walk(z, &spec, rev, offset, limit, &mut |m, s| items.push((Str::from_slice(m), s)));
            }
            if rev {
                items.reverse();
            }
            let z = zset_from_pairs(&items, lim);
            Ok(Reply::Int(store_result(cx, dst, z, "zrangestore")))
        }
    }
}

pub fn zrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zrange_generic(cx, args, 1, None, RangeType::Auto, Dir::Auto)
}
pub fn zrangestore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zrange_generic(cx, args, 2, Some(&args[1]), RangeType::Auto, Dir::Auto)
}
pub fn zrevrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zrange_generic(cx, args, 1, None, RangeType::Rank, Dir::Rev)
}
pub fn zrangebyscore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zrange_generic(cx, args, 1, None, RangeType::Score, Dir::Fwd)
}
pub fn zrevrangebyscore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zrange_generic(cx, args, 1, None, RangeType::Score, Dir::Rev)
}
pub fn zrangebylex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zrange_generic(cx, args, 1, None, RangeType::Lex, Dir::Fwd)
}
pub fn zrevrangebylex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zrange_generic(cx, args, 1, None, RangeType::Lex, Dir::Rev)
}

pub fn zcount(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let r = parse_score_range(&args[2], &args[3])?;
    let Some(z) = cx.get_zset(&args[1])? else { return Ok(Reply::Int(0)) };
    Ok(Reply::Int(z.count_in(r.nonempty(), |s, _| r.gte_min(s), |s, _| r.lte_max(s)) as i64))
}

pub fn zlexcount(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let r = parse_lex_range(&args[2], &args[3])?;
    let Some(z) = cx.get_zset(&args[1])? else { return Ok(Reply::Int(0)) };
    Ok(Reply::Int(z.count_in(r.nonempty(), |_, m| r.gte_min(m), |_, m| r.lte_max(m)) as i64))
}

// ---- single-key reads ----

pub fn zcard(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.get_zset(&args[1])?.map_or(0, |z| z.len()) as i64))
}

pub fn zscore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(match cx.get_zset(&args[1])?.and_then(|z| z.score(&args[2])) {
        Some(s) => Reply::Double(s),
        None => Reply::Nil,
    })
}

pub fn zmscore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let z = cx.get_zset(&args[1])?;
    Ok(Reply::Array(
        args[2..].iter().map(|m| z.and_then(|z| z.score(m)).map_or(Reply::Nil, Reply::Double)).collect(),
    ))
}

fn zrank_generic(cx: &mut Cx, args: &[Bytes], rev: bool) -> CmdResult {
    if args.len() > 4 {
        return Err(arity_error(cx.cmd.name));
    }
    let withscore = if args.len() == 4 {
        if !eq(&args[3], "withscore") {
            return Err(SYNTAX);
        }
        true
    } else {
        false
    };
    let missing = if withscore { Reply::NilArray } else { Reply::Nil };
    let Some(z) = cx.get_zset(&args[1])? else { return Ok(missing) };
    let len = z.len();
    Ok(match z.rank(&args[2]) {
        None => missing,
        Some((r, s)) => {
            let r = Reply::Int(if rev { len - 1 - r } else { r } as i64);
            if withscore { Reply::Array(vec![r, Reply::Double(s)]) } else { r }
        }
    })
}

pub fn zrank(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zrank_generic(cx, args, false)
}
pub fn zrevrank(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zrank_generic(cx, args, true)
}

/// ZSCAN key cursor [MATCH pattern] [COUNT count]
pub fn zscan(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let cursor = parse_cursor(&args[2])?;
    let Some(z) = cx.get_zset(&args[1])? else { return Ok(scan_reply(0, vec![])) };
    let mut count = 10usize;
    let mut pattern: Option<&Bytes> = None;
    let mut i = 3;
    while i < args.len() {
        let a = &args[i];
        let more = i + 1 < args.len();
        if eq(a, "count") && more {
            let c = int(&args[i + 1])?;
            if c < 1 {
                return Err(SYNTAX);
            }
            count = c as usize;
            i += 2;
        } else if eq(a, "match") && more {
            pattern = Some(&args[i + 1]).filter(|p| p.as_ref() != b"*");
            i += 2;
        } else if eq(a, "novalues") {
            return Err(Error::new("ERR NOVALUES option can only be used in HSCAN"));
        } else {
            return Err(SYNTAX);
        }
    }
    let mut items = Vec::new();
    let next = z.scan(cursor, count, |m, s| {
        if pattern.is_none_or(|p| glob_match(p, m, false)) {
            items.push(Reply::bulk(m));
            items.push(Reply::bulk_str(redis_proto::fmt_double(s)));
        }
    });
    Ok(scan_reply(next, items))
}

// ---- pops ----

/// The reply and, when something was popped, the key and how many.
type Popped<'k> = (Reply, Option<(&'k Bytes, usize)>);

/// `genericZpopCommand`: pop from the first existing key. `count` is -1 for a plain single pop.
/// Returns the reply and, when something was popped, the key and how many.
#[allow(clippy::too_many_arguments)]
fn zpop_generic<'k>(
    cx: &mut Cx,
    keys: &'k [Bytes],
    max: bool,
    emitkey: bool,
    count: i64,
    nested: bool,
    nil_when_empty: bool,
) -> Result<Popped<'k>, Error> {
    let mut found = None;
    for k in keys {
        match cx.peek(k) {
            None => continue,
            Some(e) if e.value.kind() != Kind::ZSet => return Err(WRONGTYPE),
            Some(_) => {
                found = Some(k);
                break;
            }
        }
    }
    let Some(key) = found else {
        return Ok((if nil_when_empty { Reply::NilArray } else { Reply::Array(vec![]) }, None));
    };
    if count == 0 {
        return Ok((Reply::Array(vec![]), None));
    }
    let count = if count == -1 { 1 } else { count as usize };
    let mut e = cx.get_mut(key).expect("checked above");
    let z = e.zset();
    let n = count.min(z.len());
    let mut items = Vec::with_capacity(n * 2);
    for _ in 0..n {
        let (m, s) = z.pop(max).expect("non-empty");
        if nested {
            items.push(Reply::Array(vec![Reply::bulk(&m), Reply::Double(s)]));
        } else {
            items.push(Reply::bulk(&m));
            items.push(Reply::Double(s));
        }
    }
    e.notify(notify::ZSET, if max { "zpopmax" } else { "zpopmin" });
    e.changes(n);
    drop(e);
    let reply = match (emitkey, nested) {
        (false, _) => Reply::Array(items),
        (true, false) => {
            let mut v = Vec::with_capacity(items.len() + 1);
            v.push(Reply::bulk(key));
            v.extend(items);
            Reply::Array(v)
        }
        (true, true) => Reply::Array(vec![Reply::bulk(key), Reply::Array(items)]),
    };
    Ok((reply, Some((key, n))))
}

fn zpop_cmd(cx: &mut Cx, args: &[Bytes], max: bool) -> CmdResult {
    if args.len() > 3 {
        return Err(SYNTAX);
    }
    let count = if args.len() == 3 { positive(&args[2], "ERR value is out of range, must be positive")? } else { -1 };
    let nested = cx.resp() >= 3 && count != -1;
    Ok(zpop_generic(cx, &args[1..2], max, false, count, nested, false)?.0)
}

pub fn zpopmin(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zpop_cmd(cx, args, false)
}
pub fn zpopmax(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    zpop_cmd(cx, args, true)
}

fn pop_name(max: bool) -> Bytes {
    b(if max { "ZPOPMAX" } else { "ZPOPMIN" })
}

/// `blockingGenericZpopCommand`.
#[allow(clippy::too_many_arguments)]
fn bzpop_generic(cx: &mut Cx, keys: &[Bytes], max: bool, timeout: i64, count: i64, nested: bool) -> CmdResult {
    // Redis only wakes zset waiters for keys that became ready *as zsets*: a write of another
    // type to a watched key keeps the client waiting rather than failing it.
    let block = |cx: &mut Cx| cx.block_on(keys.iter().map(|k| Str::from_slice(k)).collect(), timeout, Reply::NilArray);
    if cx.mode == Mode::Unblock && !keys.iter().any(|k| cx.peek(k).is_some_and(|e| e.value.kind() == Kind::ZSet)) {
        return Ok(block(cx));
    }
    for k in keys {
        match cx.peek(k) {
            None => continue,
            Some(e) if e.value.kind() != Kind::ZSet => return Err(WRONGTYPE),
            Some(_) => {}
        }
        let (reply, popped) = zpop_generic(cx, std::slice::from_ref(k), max, true, count, nested, nested)?;
        if let Some((key, n)) = popped {
            let mut p = vec![pop_name(max), key.clone()];
            if count != -1 {
                p.push(Bytes::from(util::fmt_i64(n as i64)));
            }
            cx.propagate_as(p);
        }
        return Ok(reply);
    }
    Ok(block(cx))
}

pub fn bzpopmin(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let timeout = timeout_secs(&args[args.len() - 1])?;
    bzpop_generic(cx, &args[1..args.len() - 1], false, timeout, -1, false)
}

pub fn bzpopmax(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let timeout = timeout_secs(&args[args.len() - 1])?;
    bzpop_generic(cx, &args[1..args.len() - 1], true, timeout, -1, false)
}

/// `zmpopGenericCommand`: returns (keys, max, count).
fn parse_zmpop(args: &[Bytes], numkeys_idx: usize) -> Result<(&[Bytes], bool, i64), Error> {
    let numkeys = match parse_i64(&args[numkeys_idx]) {
        Some(n) if n >= 1 => n as u64,
        _ => return Err(Error::new("ERR numkeys should be greater than 0")),
    };
    let where_idx = (numkeys_idx as u64).saturating_add(numkeys).saturating_add(1);
    if where_idx >= args.len() as u64 {
        return Err(SYNTAX);
    }
    let where_idx = where_idx as usize;
    let max = if eq(&args[where_idx], "min") {
        false
    } else if eq(&args[where_idx], "max") {
        true
    } else {
        return Err(SYNTAX);
    };
    let mut count = -1i64;
    let mut j = where_idx + 1;
    while j < args.len() {
        if count == -1 && eq(&args[j], "count") && j + 1 < args.len() {
            count = match parse_i64(&args[j + 1]) {
                Some(c) if c >= 1 => c,
                _ => return Err(Error::new("ERR count should be greater than 0")),
            };
            j += 2;
        } else {
            return Err(SYNTAX);
        }
    }
    if count == -1 {
        count = 1;
    }
    Ok((&args[numkeys_idx + 1..where_idx], max, count))
}

pub fn zmpop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (keys, max, count) = parse_zmpop(args, 1)?;
    let (reply, popped) = zpop_generic(cx, keys, max, true, count, true, true)?;
    if let Some((key, n)) = popped {
        cx.propagate_as(vec![pop_name(max), key.clone(), Bytes::from(util::fmt_i64(n as i64))]);
    }
    Ok(reply)
}

pub fn bzmpop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (keys, max, count) = parse_zmpop(args, 2)?;
    let timeout = timeout_secs(&args[1])?;
    bzpop_generic(cx, keys, max, timeout, count, true)
}

// ---- ZRANDMEMBER ----

/// Run `f` on the zset at `key` (after a read lookup) together with the engine's RNG.
fn with_zset_rng<R>(cx: &mut Cx, key: &[u8], f: impl FnOnce(&ZSet, &mut Rng) -> R) -> Result<Option<R>, Error> {
    if cx.get_zset(key)?.is_none() {
        return Ok(None);
    }
    let db = cx.client.db;
    let Engine { dbs, rng, .. } = &mut *cx.eng;
    match dbs[db].keys.get(key).map(|e| &e.value) {
        Some(Value::ZSet(z)) => Ok(Some(f(z, rng))),
        _ => Ok(None),
    }
}

/// `count` distinct ranks out of `0..size` (Floyd's algorithm), in random order.
fn distinct_ranks(rng: &mut Rng, size: usize, count: usize) -> Vec<usize> {
    let mut seen = HashSet::with_capacity(count);
    let mut out = Vec::with_capacity(count);
    for j in size - count..size {
        let t = rng.below(j + 1);
        let pick = if seen.contains(&t) { j } else { t };
        seen.insert(pick);
        out.push(pick);
    }
    // Floyd's picks are a uniform set; shuffle so the order is random too.
    for i in (1..out.len()).rev() {
        out.swap(i, rng.below(i + 1));
    }
    out
}

pub fn zrandmember(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    if args.len() < 3 {
        return Ok(with_zset_rng(cx, key, |z, rng| Reply::bulk(z.random(rng).expect("non-empty").0))?.unwrap_or(Reply::Nil));
    }
    let l = int(&args[2])?;
    if l == i64::MIN {
        return Err(Error::new(
            "ERR value is out of range, value must between -9223372036854775807 and 9223372036854775807",
        ));
    }
    if args.len() > 4 || (args.len() == 4 && !eq(&args[3], "withscores")) {
        return Err(SYNTAX);
    }
    let withscores = args.len() == 4;
    if withscores && !(-(i64::MAX / 2)..=i64::MAX / 2).contains(&l) {
        return Err(Error::new("ERR value is out of range"));
    }
    let resp3 = cx.resp() >= 3;
    let budget = cx.reply_budget();
    let reply = with_zset_rng(cx, key, |z, rng| {
        let size = z.len();
        let (count, uniq) = (l.unsigned_abs() as usize, l >= 0);
        let mut out = Vec::new();
        if count == 0 {
            return Reply::Array(out);
        }
        let mut push = |m: &[u8], s: f64| push_scored(&mut out, resp3, withscores, m, s);
        if !uniq || count == 1 {
            // With repetitions, each pick uniform, until the output buffer limit closes the client.
            let mut bytes = 0u64;
            for _ in 0..count {
                let (m, s) = z.random(rng).expect("non-empty");
                push(m, s);
                bytes += m.len() as u64 + 5 + if withscores { 4 } else { 0 };
                if bytes > budget {
                    break;
                }
            }
        } else if count >= size {
            // The whole set, in `zuiNext` order (zsets are walked from the tail).
            for (m, s) in z.iter_from(size - 1, true) {
                push(m, s);
            }
        } else if z.is_listpack() {
            // Unique picks in listpack order (selection sampling).
            let mut need = count;
            for (i, (m, s)) in z.iter().enumerate() {
                if rng.below(size - i) < need {
                    push(m, s);
                    need -= 1;
                    if need == 0 {
                        break;
                    }
                }
            }
        } else {
            for r in distinct_ranks(rng, size, count) {
                let (m, s) = z.get(r).expect("in range");
                push(m, s);
            }
        }
        Reply::Array(out)
    })?;
    Ok(reply.unwrap_or(Reply::Array(vec![])))
}

/// The reference's unstable sorts, ported so that elements comparing equal end up in the same
/// order: musl's `qsort` (smoothsort; the reference image is Alpine) and Redis' `pqsort`.
pub(crate) mod refsort {
    use std::cmp::Ordering;

    /// Rotate: `v[ar[i]] = old v[ar[i + 1]]`, `v[ar[n - 1]] = old v[ar[0]]` (musl `cycle`).
    fn cycle<T>(v: &mut [T], ar: &[usize]) {
        for w in ar.windows(2) {
            v.swap(w[0], w[1]);
        }
    }

    fn shl(p: &mut [u64; 2], mut n: u32) {
        if n >= 64 {
            n -= 64;
            p[1] = p[0];
            p[0] = 0;
            if n == 0 {
                return;
            }
        }
        p[1] <<= n;
        p[1] |= p[0] >> (64 - n);
        p[0] <<= n;
    }

    fn shr(p: &mut [u64; 2], mut n: u32) {
        if n >= 64 {
            n -= 64;
            p[0] = p[1];
            p[1] = 0;
            if n == 0 {
                return;
            }
        }
        p[0] >>= n;
        p[0] |= p[1] << (64 - n);
        p[1] >>= n;
    }

    fn pntz(p: &[u64; 2]) -> u32 {
        if p[0] != 1 {
            return (p[0] - 1).trailing_zeros();
        }
        if p[1] != 0 {
            return 64 + p[1].trailing_zeros();
        }
        0
    }

    fn sift<T>(v: &mut [T], mut head: usize, cmp: &impl Fn(&T, &T) -> Ordering, mut pshift: usize, lp: &[usize]) {
        let mut ar = vec![head];
        while pshift > 1 {
            let rt = head - 1;
            let lf = head - 1 - lp[pshift - 2];
            if cmp(&v[ar[0]], &v[lf]).is_ge() && cmp(&v[ar[0]], &v[rt]).is_ge() {
                break;
            }
            if cmp(&v[lf], &v[rt]).is_ge() {
                ar.push(lf);
                head = lf;
                pshift -= 1;
            } else {
                ar.push(rt);
                head = rt;
                pshift -= 2;
            }
        }
        cycle(v, &ar);
    }

    #[allow(clippy::too_many_arguments)]
    fn trinkle<T>(
        v: &mut [T],
        mut head: usize,
        cmp: &impl Fn(&T, &T) -> Ordering,
        pp: [u64; 2],
        mut pshift: usize,
        mut trusty: bool,
        lp: &[usize],
    ) {
        let mut p = pp;
        let mut ar = vec![head];
        while p[0] != 1 || p[1] != 0 {
            let stepson = head - lp[pshift];
            if cmp(&v[stepson], &v[ar[0]]).is_le() {
                break;
            }
            if !trusty && pshift > 1 {
                let rt = head - 1;
                let lf = head - 1 - lp[pshift - 2];
                if cmp(&v[rt], &v[stepson]).is_ge() || cmp(&v[lf], &v[stepson]).is_ge() {
                    break;
                }
            }
            ar.push(stepson);
            head = stepson;
            let trail = pntz(&p);
            shr(&mut p, trail);
            pshift += trail as usize;
            trusty = false;
        }
        if !trusty {
            cycle(v, &ar);
            sift(v, head, cmp, pshift, lp);
        }
    }

    /// musl `qsort`.
    pub fn qsort<T>(v: &mut [T], cmp: impl Fn(&T, &T) -> Ordering) {
        let size = v.len();
        if size == 0 {
            return;
        }
        let cmp = &cmp;
        let mut lp = vec![1usize, 1];
        loop {
            let n = lp[lp.len() - 2] + lp[lp.len() - 1] + 1;
            lp.push(n);
            if n >= size {
                break;
            }
        }
        let mut head = 0usize;
        let high = size - 1;
        let mut p = [1u64, 0];
        let mut pshift = 1usize;
        while head < high {
            if p[0] & 3 == 3 {
                sift(v, head, cmp, pshift, &lp);
                shr(&mut p, 2);
                pshift += 2;
            } else {
                if lp[pshift - 1] >= high - head {
                    trinkle(v, head, cmp, p, pshift, false, &lp);
                } else {
                    sift(v, head, cmp, pshift, &lp);
                }
                if pshift == 1 {
                    shl(&mut p, 1);
                    pshift = 0;
                } else {
                    shl(&mut p, pshift as u32 - 1);
                    pshift = 1;
                }
            }
            p[0] |= 1;
            head += 1;
        }
        trinkle(v, head, cmp, p, pshift, false, &lp);
        while pshift != 1 || p[0] != 1 || p[1] != 0 {
            if pshift <= 1 {
                let trail = pntz(&p);
                shr(&mut p, trail);
                pshift += trail as usize;
            } else {
                shl(&mut p, 2);
                pshift -= 2;
                p[0] ^= 7;
                shr(&mut p, 1);
                trinkle(v, head - lp[pshift] - 1, cmp, p, pshift + 1, true, &lp);
                shl(&mut p, 1);
                p[0] |= 1;
                trinkle(v, head - 1, cmp, p, pshift, true, &lp);
            }
            head -= 1;
        }
    }

    fn med3<T>(v: &[T], a: usize, b: usize, c: usize, cmp: &impl Fn(&T, &T) -> Ordering) -> usize {
        if cmp(&v[a], &v[b]).is_lt() {
            if cmp(&v[b], &v[c]).is_lt() {
                b
            } else if cmp(&v[a], &v[c]).is_lt() {
                c
            } else {
                a
            }
        } else if cmp(&v[b], &v[c]).is_gt() {
            b
        } else if cmp(&v[a], &v[c]).is_lt() {
            a
        } else {
            c
        }
    }

    fn vecswap<T>(v: &mut [T], a: usize, b: usize, n: usize) {
        for i in 0..n {
            v.swap(a + i, b + i);
        }
    }

    /// Redis `pqsort`: sorts enough that `v[lrange..=rrange]` holds the right elements in order.
    pub fn pqsort<T>(v: &mut [T], cmp: impl Fn(&T, &T) -> Ordering, lrange: usize, rrange: usize) {
        let n = v.len();
        pq(v, &cmp, 0, n, lrange, rrange);
    }

    fn pq<T>(v: &mut [T], cmp: &impl Fn(&T, &T) -> Ordering, mut a: usize, mut n: usize, lrange: usize, rrange: usize) {
        let wanted = |s: usize, len: usize| !((lrange < s && rrange < s) || (lrange >= s + len && rrange >= s + len));
        loop {
            if n < 7 {
                for pm in a + 1..a + n {
                    let mut pl = pm;
                    while pl > a && cmp(&v[pl - 1], &v[pl]).is_gt() {
                        v.swap(pl, pl - 1);
                        pl -= 1;
                    }
                }
                return;
            }
            let mut pm = a + n / 2;
            if n > 7 {
                let mut pl = a;
                let mut pn = a + n - 1;
                if n > 40 {
                    let d = n / 8;
                    pl = med3(v, pl, pl + d, pl + 2 * d, cmp);
                    pm = med3(v, pm - d, pm, pm + d, cmp);
                    pn = med3(v, pn - 2 * d, pn - d, pn, cmp);
                }
                pm = med3(v, pl, pm, pn, cmp);
            }
            v.swap(a, pm);
            let (mut pa, mut pb) = (a + 1, a + 1);
            // `pc`/`pd` may step below `a` in C; keep them as signed offsets.
            let (mut pc, mut pd) = ((a + n - 1) as isize, (a + n - 1) as isize);
            loop {
                while pb as isize <= pc {
                    let r = cmp(&v[pb], &v[a]);
                    if r.is_gt() {
                        break;
                    }
                    if r.is_eq() {
                        v.swap(pa, pb);
                        pa += 1;
                    }
                    pb += 1;
                }
                while pb as isize <= pc {
                    let r = cmp(&v[pc as usize], &v[a]);
                    if r.is_lt() {
                        break;
                    }
                    if r.is_eq() {
                        v.swap(pc as usize, pd as usize);
                        pd -= 1;
                    }
                    pc -= 1;
                }
                if pb as isize > pc {
                    break;
                }
                v.swap(pb, pc as usize);
                pb += 1;
                pc -= 1;
            }
            let pn = a + n;
            let r = (pa - a).min(pb - pa);
            vecswap(v, a, pb - r, r);
            let r = ((pd - pc) as usize).min(pn - pd as usize - 1);
            vecswap(v, pb, pn - r, r);
            let r = pb - pa;
            if r > 1 && wanted(a, r) {
                pq(v, cmp, a, r, lrange, rrange);
            }
            let r = (pd - pc) as usize;
            if r > 1 {
                a = pn - r;
                n = r;
                if wanted(a, n) {
                    continue;
                }
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, Engine};

    fn run(e: &mut Engine, c: &mut Client, line: &str) -> Reply {
        let args: Vec<Bytes> = line.split_whitespace().map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
        e.call(c, &args).unwrap_reply()
    }

    #[test]
    fn reference_sorts_sort() {
        let mut rng = Rng::seeded(7);
        for n in [0usize, 1, 2, 5, 7, 8, 40, 41, 100, 1000] {
            let base: Vec<(u32, usize)> = (0..n).map(|i| ((rng.below(20)) as u32, i)).collect();
            let mut want = base.clone();
            want.sort_by_key(|x| x.0);
            let mut v = base.clone();
            refsort::qsort(&mut v, |a, b| a.0.cmp(&b.0));
            assert_eq!(v.iter().map(|x| x.0).collect::<Vec<_>>(), want.iter().map(|x| x.0).collect::<Vec<_>>());
            let mut ids: Vec<usize> = v.iter().map(|x| x.1).collect();
            ids.sort();
            assert_eq!(ids, (0..n).collect::<Vec<_>>());
            if n > 0 {
                let k = n.div_ceil(3);
                let mut v = base.clone();
                refsort::pqsort(&mut v, |a, b| a.0.cmp(&b.0), 0, k - 1);
                assert_eq!(v[..k].iter().map(|x| x.0).collect::<Vec<_>>(), want[..k].iter().map(|x| x.0).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn floats_like_string2d() {
        assert_eq!(zfloat(b"1e400"), None);
        assert_eq!(zfloat(b"1e-400"), None);
        assert_eq!(zfloat(b"-inf"), Some(f64::NEG_INFINITY));
        assert_eq!(zfloat(b"0"), Some(0.0));
        assert_eq!(zfloat(b"0e10"), Some(0.0));
        assert_eq!(zfloat(b"4e-320"), Some(4e-320));
        assert_eq!(zfloat(b" 1"), None);
        assert_eq!(strtod_full(b""), Some(0.0));
        assert_eq!(strtod_full(b" 1"), Some(1.0));
        assert_eq!(strtod_full(b"  "), None);
        assert_eq!(strtod_full(b"1e400"), Some(f64::INFINITY));
    }

    #[test]
    fn basic_commands() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        assert_eq!(run(&mut e, &mut c, "ZADD z 1 a 2 b 3 c"), Reply::Int(3));
        assert_eq!(run(&mut e, &mut c, "ZRANGE z 0 -1"), Reply::bulks(["a", "b", "c"]));
        assert_eq!(run(&mut e, &mut c, "ZINCRBY z 5 a"), Reply::Double(6.0));
        assert_eq!(run(&mut e, &mut c, "ZRANK z a"), Reply::Int(2));
        assert_eq!(run(&mut e, &mut c, "ZCOUNT z (1 +inf"), Reply::Int(3));
        assert_eq!(run(&mut e, &mut c, "ZPOPMIN z"), Reply::Array(vec![Reply::bulk("b"), Reply::Double(2.0)]));
        assert_eq!(run(&mut e, &mut c, "ZREM z a c"), Reply::Int(2));
        assert_eq!(run(&mut e, &mut c, "EXISTS z"), Reply::Int(0));
    }

    #[test]
    fn large_zsets_use_skiplist() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        for i in 0..300 {
            run(&mut e, &mut c, &format!("ZADD z {i} m{i}"));
        }
        assert_eq!(run(&mut e, &mut c, "OBJECT ENCODING z"), Reply::bulk("skiplist"));
        assert_eq!(run(&mut e, &mut c, "ZRANK z m150"), Reply::Int(150));
        assert_eq!(run(&mut e, &mut c, "ZRANGEBYSCORE z 10 12"), Reply::bulks(["m10", "m11", "m12"]));
        assert_eq!(run(&mut e, &mut c, "ZREVRANGE z 0 1"), Reply::bulks(["m299", "m298"]));
        assert_eq!(run(&mut e, &mut c, "ZREMRANGEBYRANK z 0 99"), Reply::Int(100));
        assert_eq!(run(&mut e, &mut c, "ZCARD z"), Reply::Int(200));
        assert_eq!(run(&mut e, &mut c, "ZRANGESTORE d z 0 9"), Reply::Int(10));
        assert_eq!(run(&mut e, &mut c, "OBJECT ENCODING d"), Reply::bulk("listpack"));
    }
}
