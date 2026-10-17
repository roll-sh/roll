//! SORT and SORT_RO (`sort.c`).

use super::zsets::refsort;
use super::*;
use crate::cmd;
use crate::engine::{Effect, Mode};
use crate::notify;
use crate::types::List;
use crate::value::Value;
use std::cmp::Ordering;

pub static COMMANDS: &[Command] = &[
    cmd!("sort", -2, WRITE | DENYOOM, 1, 1, 1, keys = sort_keys, sort, Generic),
    Command { since: "7.0.0", ..cmd!("sort_ro", -2, READONLY, 1, 1, 1, keys = sort_ro_keys, sort_ro, Generic) },
];

/// `sortGetKeys`: the sorted key and the last `STORE` destination.
/// SORT_RO reads only its key (BY / GET patterns name no keys Redis reports).
fn sort_ro_keys(_args: &[Bytes]) -> Vec<usize> {
    vec![1]
}

fn sort_keys(args: &[Bytes]) -> Vec<usize> {
    let mut keys = vec![1];
    let mut store = None;
    let mut i = 2;
    while i < args.len() {
        if eq(&args[i], "limit") {
            i += 2;
        } else if eq(&args[i], "get") || eq(&args[i], "by") {
            i += 1;
        } else if eq(&args[i], "store") && i + 1 < args.len() {
            store = Some(i + 1);
        }
        i += 1;
    }
    keys.extend(store);
    keys
}

pub fn sort(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sort_generic(cx, args, false)
}

pub fn sort_ro(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sort_generic(cx, args, true)
}

struct Item {
    obj: Bytes,
    score: f64,
    /// The BY value for ALPHA sorts by pattern (`None`: the key doesn't exist).
    by: Option<Vec<u8>>,
}

/// `strcoll` in the C locale: compares up to the first NUL.
fn cstr(s: &[u8]) -> &[u8] {
    s.iter().position(|&c| c == 0).map_or(s, |i| &s[..i])
}

/// `strtod` with Redis' checks: the whole (C) string must parse, NaN and ERANGE fail.
fn sort_score(s: &[u8]) -> Option<f64> {
    let s = cstr(s);
    if s.is_empty() {
        return Some(0.0);
    }
    let start = s.iter().position(|c| !matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))?;
    let v = redis_proto::parse_double(&s[start..])?;
    (!v.is_nan()).then_some(v)
}

/// `lookupKeyByPattern`: `#` is the element itself; otherwise the first `*` is replaced by the
/// element, and `key->field` reads a hash field. Only strings (or hash fields) are found.
fn lookup_by_pattern(cx: &mut Cx, pattern: &[u8], elem: &[u8]) -> Option<Vec<u8>> {
    let pattern = cstr(pattern);
    if pattern == b"#" {
        return Some(elem.to_vec());
    }
    let star = pattern.iter().position(|&c| c == b'*')?;
    let field = pattern[star + 1..].windows(2).position(|w| w == b"->").map(|i| star + 1 + i).filter(|&f| f + 2 < pattern.len());
    let key_end = field.unwrap_or(pattern.len());
    let mut key = Vec::with_capacity(key_end + elem.len());
    key.extend_from_slice(&pattern[..star]);
    key.extend_from_slice(elem);
    key.extend_from_slice(&pattern[star + 1..key_end]);
    let now = cx.now;
    match (cx.get(&key)?, field) {
        (Value::Str(s), None) => Some(s.to_vec()),
        (Value::Hash(h), Some(f)) => h.get_live(&pattern[f + 2..], now).map(<[u8]>::to_vec),
        _ => None,
    }
}

fn sort_generic(cx: &mut Cx, args: &[Bytes], readonly: bool) -> CmdResult {
    let (mut desc, mut alpha, mut dontsort) = (false, false, false);
    let (mut limit_start, mut limit_count) = (0i64, -1i64);
    let mut sortby: Option<&Bytes> = None;
    let mut store: Option<&Bytes> = None;
    let mut gets: Vec<&Bytes> = Vec::new();
    let full_access = cx.mode == Mode::Replay || cx.client.user.clone().full_key_access(cx.cmd, args);
    let mut j = 2;
    while j < args.len() {
        let left = args.len() - j - 1;
        let a = &args[j];
        if eq(a, "asc") {
            desc = false;
        } else if eq(a, "desc") {
            desc = true;
        } else if eq(a, "alpha") {
            alpha = true;
        } else if eq(a, "limit") && left >= 2 {
            limit_start = int(&args[j + 1])?;
            limit_count = int(&args[j + 2])?;
            j += 2;
        } else if !readonly && eq(a, "store") && left >= 1 {
            store = Some(&args[j + 1]);
            j += 1;
        } else if eq(a, "by") && left >= 1 {
            sortby = Some(&args[j + 1]);
            if !cstr(&args[j + 1]).contains(&b'*') {
                dontsort = true;
            } else if !full_access {
                return Err(Error::new("ERR BY option of SORT denied due to insufficient ACL permissions."));
            }
            j += 1;
        } else if eq(a, "get") && left >= 1 {
            if !full_access {
                return Err(Error::new("ERR GET option of SORT denied due to insufficient ACL permissions."));
            }
            gets.push(&args[j + 1]);
            j += 1;
        } else {
            return Err(SYNTAX);
        }
        j += 1;
    }

    let key = &args[1];
    let (mut elems, is_set, ordered) = match cx.get(key) {
        None => (Vec::new(), false, true),
        Some(Value::List(l)) => (l.iter().map(Bytes::copy_from_slice).collect::<Vec<_>>(), false, true),
        Some(Value::Set(s)) => (s.iter_members().map(|m| Bytes::copy_from_slice(&m)).collect(), true, false),
        Some(Value::ZSet(z)) => (z.iter().map(|(m, _)| Bytes::copy_from_slice(m)).collect(), false, true),
        Some(_) => return Err(WRONGTYPE),
    };
    // Sorting a set without an order must still give replicas and scripts one answer.
    if dontsort && is_set && (store.is_some() || matches!(cx.mode, Mode::Script { .. })) {
        dontsort = false;
        alpha = true;
        sortby = None;
    }
    // `zsetConvert(…, SKIPLIST)`: SORT leaves a listpack sorted set converted.
    if let Some(mut k) = cx.get_mut(key) {
        if let Value::ZSet(z) = k.value()
            && z.is_listpack()
        {
            let n = z.len().max(1);
            z.maybe_convert(n, crate::types::zset::Limits { entries: 0, value: 0 });
        }
        k.unchanged();
    }

    let len = elems.len() as i64;
    let mut start = limit_start.clamp(0, len);
    let count = limit_count.clamp(-1, len);
    let mut end = if count < 0 { len - 1 } else { start + count - 1 };
    if start >= len {
        start = len - 1;
        end = len - 2;
    }
    end = end.min(len - 1);
    // Lists and sorted sets keep their own order when not sorting, read from the requested end.
    if dontsort && ordered && desc {
        elems.reverse();
    }

    let mut items: Vec<Item> = elems.into_iter().map(|obj| Item { obj, score: 0.0, by: None }).collect();
    let mut conversion_error = false;
    if !dontsort {
        for it in &mut items {
            let byval = match sortby {
                Some(p) => match lookup_by_pattern(cx, p, &it.obj) {
                    Some(v) => v,
                    None => continue,
                },
                None => it.obj.to_vec(),
            };
            if alpha {
                if sortby.is_some() {
                    it.by = Some(byval);
                }
            } else {
                match sort_score(&byval) {
                    Some(v) => it.score = v,
                    None => conversion_error = true,
                }
            }
        }
        let (by_pattern, storing) = (sortby.is_some(), store.is_some());
        let cmp = |a: &Item, b: &Item| -> Ordering {
            let c = if !alpha {
                a.score.partial_cmp(&b.score).unwrap_or(Ordering::Equal).then_with(|| a.obj.cmp(&b.obj))
            } else if by_pattern {
                match (&a.by, &b.by) {
                    (Some(x), Some(y)) if storing => x.cmp(y),
                    (Some(x), Some(y)) => cstr(x).cmp(cstr(y)),
                    (None, None) => Ordering::Equal,
                    (None, _) => Ordering::Less,
                    _ => Ordering::Greater,
                }
            } else if storing {
                a.obj.cmp(&b.obj)
            } else {
                cstr(&a.obj).cmp(cstr(&b.obj))
            };
            if desc { c.reverse() } else { c }
        };
        if by_pattern && (start != 0 || end != len - 1) && len > 0 {
            refsort::pqsort(&mut items, cmp, start.max(0) as usize, end.max(0) as usize);
        } else {
            refsort::qsort(&mut items, cmp);
        }
    }

    let picked = if end >= start { &items[start as usize..=end as usize] } else { &[][..] };
    let outputlen = if gets.is_empty() { picked.len() } else { gets.len() * picked.len() };
    if conversion_error {
        return Err(Error::new("ERR One or more scores can't be converted into double"));
    }
    let Some(dst) = store else {
        let mut out = Vec::with_capacity(outputlen);
        for it in picked {
            if gets.is_empty() {
                out.push(Reply::Bulk(it.obj.clone()));
            }
            for g in &gets {
                out.push(lookup_by_pattern(cx, g, &it.obj).map_or(Reply::Nil, |v| Reply::Bulk(v.into())));
            }
        }
        return Ok(Reply::Array(out));
    };

    let mut values: Vec<Bytes> = Vec::with_capacity(outputlen);
    for it in picked {
        if gets.is_empty() {
            values.push(it.obj.clone());
        }
        for g in &gets {
            values.push(lookup_by_pattern(cx, g, &it.obj).map(Bytes::from).unwrap_or_default());
        }
    }
    // The stored list is replicated as its contents: the order of ties (and of a set read as
    // is) depends on the source's encoding, which a replica or a restore needn't share.
    cx.no_propagate();
    let db = cx.db_index();
    if !values.is_empty() {
        let list = List::from_items(&values, cx.cfg().list_max_listpack_size);
        cx.set(dst, Value::List(Box::new(list)), false);
        cx.notify(notify::LIST, "sortstore", dst);
        propagate_list(cx, db, dst, values);
    } else if cx.delete(dst) {
        cx.notify(notify::GENERIC, "del", dst);
        cx.also_propagate(db, vec![Bytes::from_static(b"DEL"), dst.clone()]);
    }
    Ok(Reply::Int(outputlen as i64))
}

/// `DEL dst` + `RPUSH dst …`, in MULTI/EXEC at the top level.
fn propagate_list(cx: &mut Cx, db: usize, dst: &Bytes, values: Vec<Bytes>) {
    let top = matches!(cx.mode, Mode::Top | Mode::Unblock);
    let wrap = |cx: &mut Cx, name: &'static [u8]| {
        if top {
            cx.eng.effects.push(Effect { db: db as u16, args: vec![Bytes::from_static(name)] });
        }
    };
    wrap(cx, b"MULTI");
    cx.also_propagate(db, vec![Bytes::from_static(b"DEL"), dst.clone()]);
    let mut push = Vec::with_capacity(values.len() + 2);
    push.push(Bytes::from_static(b"RPUSH"));
    push.push(dst.clone());
    push.extend(values);
    cx.also_propagate(db, push);
    wrap(cx, b"EXEC");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, Engine};

    fn call(e: &mut Engine, c: &mut Client, line: &str) -> Reply {
        let args: Vec<Bytes> = line.split(' ').map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
        e.call(c, &args).unwrap_reply()
    }

    fn bulks(v: &[&str]) -> Reply {
        Reply::Array(v.iter().map(|s| Reply::bulk(s.as_bytes())).collect())
    }

    #[test]
    fn keys_include_the_last_store() {
        let a = |s: &str| s.split(' ').map(|x| Bytes::copy_from_slice(x.as_bytes())).collect::<Vec<_>>();
        assert_eq!(sort_keys(&a("SORT k LIMIT 0 1 BY w_* GET # STORE d1 STORE d2")), vec![1, 12]);
        assert_eq!(sort_keys(&a("SORT k GET store")), vec![1]);
    }

    #[test]
    fn store_replicates_as_the_list() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        call(&mut e, &mut c, "SADD s c a b");
        e.effects.clear();
        assert_eq!(call(&mut e, &mut c, "SORT s BY nosort STORE d"), Reply::Int(3));
        let fx: Vec<String> = e.effects.iter().map(|f| String::from_utf8_lossy(&f.args.concat()).into_owned()).collect();
        assert_eq!(fx, ["MULTI", "DELd", "RPUSHdabc", "EXEC"]);
        assert_eq!(call(&mut e, &mut c, "LRANGE d 0 -1"), bulks(&["a", "b", "c"]));
        let mut r = Engine::standalone();
        r.apply_all(&e.effects);
        assert_eq!(call(&mut r, &mut c, "LRANGE d 0 -1"), bulks(&["a", "b", "c"]));
    }
}
