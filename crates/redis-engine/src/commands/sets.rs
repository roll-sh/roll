//! Set commands.

use super::*;
use crate::cmd;
use crate::commands::keys::{parse_cursor, scan_reply};
use crate::engine::Mode;
use crate::notify;
use crate::types::Set;
use crate::types::set::Limits;
use crate::util::{Rng, glob_match, parse_i64};
use crate::value::{Entry, Kind, Value};
use std::borrow::Cow;
use std::collections::HashSet;

pub static COMMANDS: &[Command] = &[
    cmd!("sadd", -3, WRITE | DENYOOM | FAST, 1, 1, 1, sadd, Set),
    cmd!("srem", -3, WRITE | FAST, 1, 1, 1, srem, Set),
    cmd!("smembers", 2, READONLY, 1, 1, 1, smembers, Set),
    cmd!("sismember", 3, READONLY | FAST, 1, 1, 1, sismember, Set),
    cmd!("smismember", -3, READONLY | FAST, 1, 1, 1, smismember, Set),
    cmd!("scard", 2, READONLY | FAST, 1, 1, 1, scard, Set),
    cmd!("spop", -2, WRITE | FAST, 1, 1, 1, spop, Set),
    cmd!("srandmember", -2, READONLY, 1, 1, 1, srandmember, Set),
    cmd!("smove", 4, WRITE | FAST, 1, 2, 1, smove, Set),
    cmd!("sinter", -2, READONLY, 1, -1, 1, sinter, Set),
    cmd!("sintercard", -3, READONLY, keys = numkeys_keys, sintercard, Set),
    cmd!("sinterstore", -3, WRITE | DENYOOM, 1, -1, 1, sinterstore, Set),
    cmd!("sunion", -2, READONLY, 1, -1, 1, sunion, Set),
    cmd!("sunionstore", -3, WRITE | DENYOOM, 1, -1, 1, sunionstore, Set),
    cmd!("sunioncard", -3, READONLY, keys = numkeys_keys, sunioncard, Set),
    cmd!("sdiff", -2, READONLY, 1, -1, 1, sdiff, Set),
    cmd!("sdiffstore", -3, WRITE | DENYOOM, 1, -1, 1, sdiffstore, Set),
    cmd!("sdiffcard", -3, READONLY, keys = numkeys_keys, sdiffcard, Set),
    cmd!("sscan", -3, READONLY, 1, 1, 1, sscan, Set),
];

fn b(s: &'static str) -> Bytes {
    Bytes::from_static(s.as_bytes())
}

fn limits(cx: &Cx) -> Limits {
    Limits::from_cfg(cx.cfg())
}

/// A private RNG stream derived from the engine's (so it can be used while a key is borrowed).
fn rng(cx: &mut Cx) -> Rng {
    Rng::seeded(cx.eng.rng.next_u64())
}

fn members_replies(s: &Set) -> Vec<Reply> {
    s.iter_members().map(|m| Reply::bulk(&*m)).collect()
}

/// Key positions of `CMD numkeys key [key ...] ...`.
fn numkeys_keys(args: &[Bytes]) -> Vec<usize> {
    let Some(n) = args.get(1).and_then(|a| parse_i64(a)) else { return vec![] };
    (0..n.max(0) as usize).map(|i| 2 + i).filter(|&i| i < args.len()).collect()
}

pub fn sadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let lim = limits(cx);
    let hint = args.len() - 2;
    let mut e = cx.get_or_create(&args[1], Kind::Set)?;
    let s = e.set();
    if s.is_empty() {
        *s = Set::create(&args[2], hint, &lim);
    } else {
        s.maybe_convert(hint, &lim);
    }
    let mut added = 0;
    for m in &args[2..] {
        if s.add(m, &lim) {
            added += 1;
        }
    }
    if added > 0 {
        e.notify(notify::SET, "sadd");
        e.changes(added as usize);
    } else {
        e.unchanged();
    }
    Ok(Reply::Int(added))
}

pub fn srem(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let Some(mut e) = cx.get_mut_typed(&args[1], Kind::Set)? else { return Ok(Reply::Int(0)) };
    let s = e.set();
    let mut n = 0;
    for m in &args[2..] {
        if s.remove(m) {
            n += 1;
            if s.is_empty() {
                break;
            }
        }
    }
    if n > 0 {
        e.notify(notify::SET, "srem");
        e.changes(n as usize);
    } else {
        e.unchanged();
    }
    Ok(Reply::Int(n))
}

pub fn smembers(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Set(cx.get_set(&args[1])?.map(members_replies).unwrap_or_default()))
}

pub fn sismember(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.get_set(&args[1])?.is_some_and(|s| s.contains(&args[2])) as i64))
}

pub fn smismember(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let s = cx.get_set(&args[1])?;
    Ok(Reply::Array(args[2..].iter().map(|m| Reply::Int(s.is_some_and(|s| s.contains(m)) as i64)).collect()))
}

pub fn scard(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.get_set(&args[1])?.map_or(0, |s| s.len()) as i64))
}

/// Propagate popped members as `SREM key m…` in batches of 1024 (wrapped in MULTI/EXEC when
/// there are several), instead of the command itself.
fn propagate_srem(cx: &mut Cx, key: &Bytes, members: &[Vec<u8>]) {
    cx.no_propagate();
    if cx.mode == Mode::Replay || members.is_empty() {
        return;
    }
    let db = cx.db_index();
    let start = cx.eng.effects.len();
    for chunk in members.chunks(1024) {
        let mut a = Vec::with_capacity(2 + chunk.len());
        a.push(b("SREM"));
        a.push(key.clone());
        a.extend(chunk.iter().map(|m| Bytes::copy_from_slice(m)));
        cx.also_propagate(db, a);
    }
    if cx.mode == Mode::Top || cx.mode == Mode::Unblock {
        crate::commands::multi::wrap_effects(cx, start);
    }
}

/// SPOP key [count]
pub fn spop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() == 3 {
        return spop_count(cx, args);
    }
    if args.len() > 3 {
        return Err(SYNTAX);
    }
    let key = &args[1];
    let mut rng = rng(cx);
    let Some(mut e) = cx.get_mut_typed(key, Kind::Set)? else { return Ok(Reply::Nil) };
    let m = e.set().pop_random(&mut rng).unwrap_or_default();
    e.notify(notify::SET, "spop");
    drop(e);
    let mb = Bytes::from(m);
    cx.propagate_as(vec![b("SREM"), key.clone(), mb.clone()]);
    Ok(Reply::Bulk(mb))
}

const MUST_BE_POSITIVE: Error = Error::new("ERR value is out of range, must be positive");

fn spop_count(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let count = int(&args[2]).map_err(|_| MUST_BE_POSITIVE)?;
    if count < 0 {
        return Err(MUST_BE_POSITIVE);
    }
    let count = count as u64;
    let key = &args[1];
    let lim = limits(cx);
    let lazy = cx.cfg().lazyfree_lazy_server_del;
    let mut rng = rng(cx);
    let Some(mut e) = cx.get_mut_typed(key, Kind::Set)? else { return Ok(Reply::Set(vec![])) };
    if count == 0 {
        e.unchanged();
        return Ok(Reply::Set(vec![]));
    }
    let size = e.set().len() as u64;
    e.notify(notify::SET, "spop");
    e.changes(count.min(size) as usize);
    let s = e.set();
    if count >= size {
        // The whole set: the key goes away (the guard deletes it, emitting `del`).
        let out = members_replies(s);
        *s = Set::default();
        drop(e);
        cx.propagate_as(vec![b(if lazy { "UNLINK" } else { "DEL" }), key.clone()]);
        return Ok(Reply::Set(out));
    }
    let remaining = size - count;
    let popped: Vec<Vec<u8>> = if remaining.saturating_mul(5) > count {
        // Few elements: extract them one by one.
        (0..count).filter_map(|_| s.pop_random(&mut rng)).collect()
    } else {
        // Most of the set: move the survivors to a new set (Redis rebuilds it, which can change
        // the encoding) and return what is left of the old one.
        let mut keep = if s.is_intset() { Set::new_intset() } else { Set::new_listpack() };
        for _ in 0..remaining {
            let m = s.random_member(&mut rng);
            keep.add(&m, &lim);
            s.remove(&m);
        }
        let out = s.iter_members().map(Cow::into_owned).collect();
        *s = keep;
        out
    };
    s.shrink_to_fit();
    drop(e);
    let reply = popped.iter().map(Reply::bulk).collect();
    propagate_srem(cx, key, &popped);
    Ok(Reply::Set(reply))
}

/// SRANDMEMBER key [count]
pub fn srandmember(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() == 3 {
        return srandmember_count(cx, args);
    }
    if args.len() > 3 {
        return Err(SYNTAX);
    }
    let mut rng = rng(cx);
    Ok(match cx.get_set(&args[1])? {
        Some(s) => Reply::bulk(s.random_member(&mut rng)),
        None => Reply::Nil,
    })
}

fn srandmember_count(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let l = int(&args[2])?;
    if l == i64::MIN {
        return Err(Error::new(
            "ERR value is out of range, value must between -9223372036854775807 and 9223372036854775807",
        ));
    }
    let (count, uniq) = if l >= 0 { (l as u64, true) } else { (l.unsigned_abs(), false) };
    let mut rng = rng(cx);
    let budget = cx.reply_budget();
    let Some(s) = cx.get_set(&args[1])? else { return Ok(Reply::Array(vec![])) };
    if count == 0 {
        return Ok(Reply::Array(vec![]));
    }
    // Repetitions allowed: independent draws, until the output buffer limit closes the client.
    if !uniq || count == 1 {
        let (mut out, mut bytes) = (Vec::new(), 0u64);
        for _ in 0..count {
            let m = s.random_member(&mut rng);
            bytes += m.len() as u64 + 5;
            out.push(Reply::bulk(m));
            if bytes > budget {
                break;
            }
        }
        return Ok(Reply::Array(out));
    }
    let size = s.len() as u64;
    // The whole set, in its own order.
    if count >= size {
        return Ok(Reply::Array(members_replies(s)));
    }
    let count = count as usize;
    let out: Vec<Vec<u8>> = if s.is_hashtable() && count * 3 <= s.len() {
        // Few of many: random draws until enough distinct members.
        let mut seen = HashSet::with_capacity(count);
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            let m = s.random_member(&mut rng);
            if seen.insert(m.clone()) {
                out.push(m);
            }
        }
        out
    } else {
        // Most of the set: copy it and drop random members.
        let mut all: Vec<Vec<u8>> = s.iter_members().map(Cow::into_owned).collect();
        while all.len() > count {
            let i = rng.below(all.len());
            all.swap_remove(i);
        }
        all
    };
    Ok(Reply::bulks(out))
}

pub fn smove(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (src, dst, m) = (&args[1], &args[2], &args[3]);
    let lim = limits(cx);
    let src_kind = cx.peek(src).map(|e| e.value.kind());
    let dst_kind = cx.peek(dst).map(|e| e.value.kind());
    let Some(sk) = src_kind else { return Ok(Reply::Int(0)) };
    if sk != Kind::Set || dst_kind.is_some_and(|k| k != Kind::Set) {
        return Err(WRONGTYPE);
    }
    if src == dst {
        return Ok(Reply::Int(cx.get_set(src)?.is_some_and(|s| s.contains(m)) as i64));
    }
    {
        let mut e = cx.get_mut(src).expect("checked above");
        if !e.set().remove(m) {
            e.unchanged();
            return Ok(Reply::Int(0));
        }
        e.notify(notify::SET, "srem");
    }
    let mut e = cx.get_or_create(dst, Kind::Set)?;
    if e.set().is_empty() {
        *e.set() = Set::create(m, 1, &lim);
    }
    if e.set().add(m, &lim) {
        e.notify(notify::SET, "sadd");
    } else {
        e.unchanged();
    }
    Ok(Reply::Int(1))
}

// ---- multi-key operations ----

/// Read-lookup every key as a set (WRONGTYPE at the first non-set). Returns which exist.
fn lookup_sets(cx: &mut Cx, keys: &[Bytes]) -> Result<Vec<bool>, Error> {
    keys.iter().map(|k| Ok(cx.get_set(k)?.is_some())).collect()
}

/// Shared borrows of the sets found by [`lookup_sets`].
fn borrow_sets<'a>(cx: &'a Cx, keys: &[Bytes], exists: &[bool]) -> Vec<Option<&'a Set>> {
    let db = &cx.eng.dbs[cx.client.db];
    keys.iter()
        .zip(exists)
        .map(|(k, &ok)| match db.keys.get(k) {
            Some(Entry { value: Value::Set(s), .. }) if ok => Some(&**s),
            _ => None,
        })
        .collect()
}

/// Store a computed set into `dst` (`setKey`), or delete `dst` when the result is empty.
fn store_result(cx: &mut Cx, dst: &Bytes, mut set: Set, event: &'static str) -> CmdResult {
    let n = set.len();
    if n > 0 {
        set.shrink_to_fit();
        cx.set(dst, Value::Set(Box::new(set)), false);
        cx.notify(notify::SET, event, dst);
    } else if cx.delete(dst) {
        cx.notify(notify::GENERIC, "del", dst);
    }
    Ok(Reply::Int(n as i64))
}

/// `qsort` as implemented by musl (smoothsort; the reference server runs on Alpine). Redis sorts
/// SINTER inputs by cardinality with it, and its tie-breaking decides the output order.
fn musl_qsort<T: Copy>(v: &mut [T], cmp: impl Fn(&T, &T) -> std::cmp::Ordering) {
    let nel = v.len();
    if nel == 0 {
        return;
    }
    let ge = |a: usize, b: usize, v: &[T]| cmp(&v[a], &v[b]) != std::cmp::Ordering::Less;
    let le = |a: usize, b: usize, v: &[T]| cmp(&v[a], &v[b]) != std::cmp::Ordering::Greater;
    // Leonardo numbers (in elements).
    let mut lp = vec![1usize, 1];
    while {
        let n = lp[lp.len() - 2] + lp[lp.len() - 1] + 1;
        lp.push(n);
        n < nel
    } {}
    for _ in 0..3 {
        let n = lp[lp.len() - 2] + lp[lp.len() - 1] + 1;
        lp.push(n);
    }
    fn cycle<T: Copy>(v: &mut [T], ar: &[usize]) {
        if ar.len() < 2 {
            return;
        }
        let tmp = v[ar[0]];
        for i in 0..ar.len() - 1 {
            v[ar[i]] = v[ar[i + 1]];
        }
        v[ar[ar.len() - 1]] = tmp;
    }
    let sift = |v: &mut [T], mut head: usize, mut pshift: usize, lp: &[usize]| {
        let mut ar = vec![head];
        while pshift > 1 {
            let rt = head - 1;
            let lf = head - 1 - lp[pshift - 2];
            if ge(ar[0], lf, v) && ge(ar[0], rt, v) {
                break;
            }
            if ge(lf, rt, v) {
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
    };
    let pntz = |p: u128| (p - 1).trailing_zeros() as usize;
    let trinkle = |v: &mut [T], mut head: usize, pp: u128, mut pshift: usize, mut trusty: bool, lp: &[usize]| {
        let mut p = pp;
        let mut ar = vec![head];
        while p != 1 {
            let stepson = head - lp[pshift];
            if le(stepson, ar[0], v) {
                break;
            }
            if !trusty && pshift > 1 {
                let rt = head - 1;
                let lf = head - 1 - lp[pshift - 2];
                if ge(rt, stepson, v) || ge(lf, stepson, v) {
                    break;
                }
            }
            ar.push(stepson);
            head = stepson;
            let trail = pntz(p);
            p >>= trail;
            pshift += trail;
            trusty = false;
        }
        if !trusty {
            cycle(v, &ar);
            sift(v, head, pshift, lp);
        }
    };
    let mut head = 0usize;
    let high = nel - 1;
    let mut p: u128 = 1;
    let mut pshift = 1usize;
    while head < high {
        if p & 3 == 3 {
            sift(v, head, pshift, &lp);
            p >>= 2;
            pshift += 2;
        } else {
            if lp[pshift - 1] >= high - head {
                trinkle(v, head, p, pshift, false, &lp);
            } else {
                sift(v, head, pshift, &lp);
            }
            if pshift == 1 {
                p <<= 1;
                pshift = 0;
            } else {
                p <<= pshift - 1;
                pshift = 1;
            }
        }
        p |= 1;
        head += 1;
    }
    trinkle(v, head, p, pshift, false, &lp);
    while pshift != 1 || p != 1 {
        if pshift <= 1 {
            let trail = pntz(p);
            p >>= trail;
            pshift += trail;
        } else {
            p <<= 2;
            pshift -= 2;
            p ^= 7;
            p >>= 1;
            trinkle(v, head - lp[pshift] - 1, p, pshift + 1, true, &lp);
            p <<= 1;
            p |= 1;
            trinkle(v, head - 1, p, pshift, true, &lp);
        }
        head -= 1;
    }
}

enum InterOut {
    Card(u64),
    Members(Vec<Reply>),
    Store(Set),
}

/// SINTER / SINTERSTORE / SINTERCARD (`sinterGenericCommand`).
fn sinter_generic(cx: &mut Cx, keys: &[Bytes], dst: Option<&Bytes>, card_only: bool, limit: u64) -> CmdResult {
    let exists = lookup_sets(cx, keys)?;
    if exists.iter().any(|e| !e) {
        // Intersection with an empty set.
        if let Some(d) = dst {
            if cx.delete(d) {
                cx.notify(notify::GENERIC, "del", d);
            }
            return Ok(Reply::Int(0));
        }
        return Ok(if card_only { Reply::Int(0) } else { Reply::Set(vec![]) });
    }
    let lim = limits(cx);
    let out = {
        let mut sets: Vec<&Set> = borrow_sets(cx, keys, &exists).into_iter().flatten().collect();
        musl_qsort(&mut sets, |a, b| a.len().cmp(&b.len()));
        let first = sets[0];
        let mut store = dst.map(|_| if first.is_intset() { Set::new_intset() } else { Set::new_listpack() });
        let mut only_ints = true;
        let mut card = 0u64;
        let mut members = Vec::new();
        for m in first.iter_members() {
            if !sets[1..].iter().all(|s| std::ptr::eq(*s, first) || s.contains(&m)) {
                continue;
            }
            if card_only {
                card += 1;
                if limit > 0 && card >= limit {
                    break;
                }
            } else if let Some(d) = store.as_mut() {
                if only_ints && parse_i64(&m).is_none() {
                    only_ints = false;
                }
                d.add(&m, &lim);
            } else {
                members.push(Reply::bulk(&*m));
            }
        }
        match store {
            Some(mut d) => {
                if only_ints {
                    d.to_intset_if_possible(&lim);
                }
                InterOut::Store(d)
            }
            None if card_only => InterOut::Card(card),
            None => InterOut::Members(members),
        }
    };
    match out {
        InterOut::Card(n) => Ok(Reply::Int(n as i64)),
        InterOut::Members(m) => Ok(Reply::Set(m)),
        InterOut::Store(set) => store_result(cx, dst.unwrap(), set, "sinterstore"),
    }
}

pub fn sinter(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sinter_generic(cx, &args[1..], None, false, 0)
}

pub fn sinterstore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sinter_generic(cx, &args[2..], Some(&args[1]), false, 0)
}

const NUMKEYS_ERR: Error = Error::new("ERR numkeys should be greater than 0");
const LIMIT_ERR: Error = Error::new("ERR LIMIT can't be negative");

/// `numkeys key [key …]`: the number of keys, validated against the arguments.
fn parse_numkeys(args: &[Bytes]) -> Result<usize, Error> {
    let n = int(&args[1]).map_err(|_| NUMKEYS_ERR)?;
    if n < 1 {
        return Err(NUMKEYS_ERR);
    }
    if n as u64 > (args.len() - 2) as u64 {
        return Err(Error::new("ERR Number of keys can't be greater than number of args"));
    }
    Ok(n as usize)
}

/// Options after the keys of the `*CARD` commands: `LIMIT n` and (SUNIONCARD) `APPROX`.
fn parse_card_opts(opts: &[Bytes], allow_approx: bool) -> Result<(u64, bool), Error> {
    let (mut limit, mut approx) = (0u64, false);
    let mut i = 0;
    while i < opts.len() {
        if eq(&opts[i], "limit") && i + 1 < opts.len() {
            let l = int(&opts[i + 1]).map_err(|_| LIMIT_ERR)?;
            if l < 0 {
                return Err(LIMIT_ERR);
            }
            limit = l as u64;
            i += 2;
        } else if allow_approx && eq(&opts[i], "approx") {
            approx = true;
            i += 1;
        } else {
            return Err(SYNTAX);
        }
    }
    Ok((limit, approx))
}

/// SINTERCARD numkeys key [key …] [LIMIT limit]
pub fn sintercard(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let n = parse_numkeys(args)?;
    let (limit, _) = parse_card_opts(&args[2 + n..], false)?;
    sinter_generic(cx, &args[2..2 + n], None, true, limit)
}

/// SUNION / SDIFF and their STORE variants (`sunionDiffGenericCommand`).
fn sunion_diff(cx: &mut Cx, keys: &[Bytes], dst: Option<&Bytes>, diff: bool) -> CmdResult {
    let exists = lookup_sets(cx, keys)?;
    let lim = limits(cx);
    let res = {
        let sets = borrow_sets(cx, keys, &exists);
        let all_ints = sets.iter().flatten().all(|s| s.is_intset());
        let mut res = if dst.is_some() || all_ints { Set::new_intset() } else { Set::new_hashtable(0) };
        if !diff {
            for s in sets.iter().flatten() {
                for m in s.iter_members() {
                    res.add(&m, &lim);
                }
            }
        } else if let Some(first) = sets[0] {
            let sameset = keys[1..].iter().zip(&sets[1..]).any(|(k, s)| s.is_some() && *k == keys[0]);
            if !sameset {
                // Algorithm 1 is O(N*M) over the first set, 2 is O(total); Redis picks by size.
                let (mut one, mut two) = (0usize, 0usize);
                for s in sets.iter().flatten() {
                    one += first.len();
                    two += s.len();
                }
                if one / 2 <= two {
                    for m in first.iter_members() {
                        if !sets[1..].iter().flatten().any(|s| s.contains(&m)) {
                            res.add(&m, &lim);
                        }
                    }
                } else {
                    for (j, s) in sets.iter().enumerate() {
                        let Some(s) = s else { continue };
                        for m in s.iter_members() {
                            if j == 0 {
                                res.add(&m, &lim);
                            } else {
                                res.remove(&m);
                            }
                        }
                        if res.is_empty() {
                            break;
                        }
                    }
                }
            }
        }
        res
    };
    match dst {
        None => Ok(Reply::Set(members_replies(&res))),
        Some(d) => store_result(cx, d, res, if diff { "sdiffstore" } else { "sunionstore" }),
    }
}

pub fn sunion(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sunion_diff(cx, &args[1..], None, false)
}

pub fn sunionstore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sunion_diff(cx, &args[2..], Some(&args[1]), false)
}

pub fn sdiff(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sunion_diff(cx, &args[1..], None, true)
}

pub fn sdiffstore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sunion_diff(cx, &args[2..], Some(&args[1]), true)
}

/// SDIFFCARD numkeys key [key …] [LIMIT limit]
pub fn sdiffcard(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let n = parse_numkeys(args)?;
    let (limit, _) = parse_card_opts(&args[2 + n..], false)?;
    let keys = &args[2..2 + n];
    let exists = lookup_sets(cx, keys)?;
    let sets = borrow_sets(cx, keys, &exists);
    let Some(first) = sets[0] else { return Ok(Reply::Int(0)) };
    if keys[1..].iter().zip(&sets[1..]).any(|(k, s)| s.is_some() && *k == keys[0]) {
        return Ok(Reply::Int(0));
    }
    let mut card = 0u64;
    for m in first.iter_members() {
        if !sets[1..].iter().flatten().any(|s| s.contains(&m)) {
            card += 1;
            if limit > 0 && card >= limit {
                break;
            }
        }
    }
    Ok(Reply::Int(card as i64))
}

/// SUNIONCARD numkeys key [key …] [APPROX] [LIMIT limit]
pub fn sunioncard(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let n = parse_numkeys(args)?;
    let (limit, approx) = parse_card_opts(&args[2 + n..], true)?;
    let keys = &args[2..2 + n];
    let exists = lookup_sets(cx, keys)?;
    let sets = borrow_sets(cx, keys, &exists);
    let card = if approx {
        // A HyperLogLog estimate over every member.
        let mut regs = super::hll::Registers::new();
        for s in sets.iter().flatten() {
            for m in s.iter_members() {
                regs.add(&m);
            }
        }
        let c = regs.count();
        if limit > 0 { c.min(limit) } else { c }
    } else {
        let mut seen: HashSet<Cow<[u8]>> = HashSet::new();
        'outer: for s in sets.iter().flatten() {
            for m in s.iter_members() {
                seen.insert(m);
                if limit > 0 && seen.len() as u64 >= limit {
                    break 'outer;
                }
            }
        }
        seen.len() as u64
    };
    Ok(Reply::Int(card as i64))
}

/// SSCAN key cursor [MATCH pattern] [COUNT count]
pub fn sscan(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let cursor = parse_cursor(&args[2])?;
    let Some(set) = cx.get_set(&args[1])? else { return Ok(scan_reply(0, vec![])) };
    let mut count = 10usize;
    let mut pattern: Option<&[u8]> = None;
    let mut i = 3;
    while i < args.len() {
        let more = i + 1 < args.len();
        if eq(&args[i], "count") && more {
            let c = int(&args[i + 1])?;
            if c < 1 {
                return Err(SYNTAX);
            }
            count = c as usize;
            i += 2;
        } else if eq(&args[i], "match") && more {
            let p = &args[i + 1][..];
            pattern = if p == b"*" { None } else { Some(p) };
            i += 2;
        } else if eq(&args[i], "novalues") {
            return Err(Error::new("ERR NOVALUES option can only be used in HSCAN"));
        } else {
            return Err(SYNTAX);
        }
    }
    let mut found: Vec<Vec<u8>> = Vec::new();
    let mut c = cursor;
    if set.is_hashtable() {
        let mut iterations = count.saturating_mul(10);
        loop {
            c = set.scan(c, |m| found.push(m.to_vec()));
            iterations -= 1;
            if c == 0 || iterations == 0 || found.len() >= count {
                break;
            }
        }
    } else {
        c = set.scan(0, |m| found.push(m.to_vec()));
    }
    let items = found
        .into_iter()
        .filter(|m| pattern.is_none_or(|p| glob_match(p, m, false)))
        .map(Reply::bulk)
        .collect();
    Ok(scan_reply(c, items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, Engine};

    fn call(e: &mut Engine, c: &mut Client, args: &[&str]) -> Reply {
        let a: Vec<Bytes> = args.iter().map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
        e.call(c, &a).unwrap_reply()
    }

    fn bulks(v: &[&str]) -> Vec<Reply> {
        v.iter().map(Reply::bulk).collect()
    }

    #[test]
    fn musl_qsort_ties() {
        // Observed on redis:8-alpine: which of several equal-size sets SINTER iterates.
        let order = |sizes: &[usize]| {
            let mut v: Vec<(usize, usize)> = sizes.iter().copied().enumerate().collect();
            musl_qsort(&mut v, |a, b| a.1.cmp(&b.1));
            v[0].0
        };
        assert_eq!(order(&[3, 3]), 0);
        assert_eq!(order(&[3, 3, 3]), 0);
        assert_eq!(order(&[4, 3, 3, 3]), 2);
        assert_eq!(order(&[4, 4, 3, 3, 3, 4]), 2);
        let mut v: Vec<i32> = (0..200).map(|i| (i * 7919) % 211).collect();
        let mut w = v.clone();
        musl_qsort(&mut v, |a, b| a.cmp(b));
        w.sort();
        assert_eq!(v, w);
    }

    #[test]
    fn basic_set_ops() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        assert_eq!(call(&mut e, &mut c, &["SADD", "a", "3", "1", "2", "1"]), Reply::Int(3));
        assert_eq!(call(&mut e, &mut c, &["SMEMBERS", "a"]), Reply::Set(bulks(&["1", "2", "3"])));
        assert_eq!(call(&mut e, &mut c, &["OBJECT", "ENCODING", "a"]), Reply::bulk("intset"));
        call(&mut e, &mut c, &["SADD", "b", "2", "x", "3"]);
        assert_eq!(call(&mut e, &mut c, &["SINTER", "a", "b"]), Reply::Set(bulks(&["2", "3"])));
        assert_eq!(call(&mut e, &mut c, &["SINTERSTORE", "d", "b", "a"]), Reply::Int(2));
        assert_eq!(call(&mut e, &mut c, &["OBJECT", "ENCODING", "d"]), Reply::bulk("intset"));
        assert_eq!(call(&mut e, &mut c, &["SDIFFCARD", "2", "b", "a"]), Reply::Int(1));
        assert_eq!(call(&mut e, &mut c, &["SUNIONCARD", "2", "b", "a"]), Reply::Int(4));
        assert!(!call(&mut e, &mut c, &["SPOP", "a", "10"]).is_error());
        assert_eq!(call(&mut e, &mut c, &["EXISTS", "a"]), Reply::Int(0));
        assert_eq!(call(&mut e, &mut c, &["SMOVE", "b", "n", "x"]), Reply::Int(1));
        assert_eq!(call(&mut e, &mut c, &["SMEMBERS", "n"]), Reply::Set(bulks(&["x"])));
    }

    #[test]
    fn spop_propagates_srem() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        call(&mut e, &mut c, &["SADD", "s", "a", "b", "c", "d", "e", "f"]);
        e.take_effects();
        let r = call(&mut e, &mut c, &["SPOP", "s", "2"]);
        let Reply::Set(items) = r else { panic!() };
        assert_eq!(items.len(), 2);
        let fx = e.take_effects();
        assert_eq!(fx.len(), 1);
        assert_eq!(&fx[0].args[0][..], b"SREM");
        assert_eq!(fx[0].args.len(), 4);
        call(&mut e, &mut c, &["SPOP", "s"]);
        let fx = e.take_effects();
        assert_eq!(&fx[0].args[0][..], b"SREM");
    }
}
