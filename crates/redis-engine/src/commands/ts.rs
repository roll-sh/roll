//! Time series commands (TS.*), as the RedisTimeSeries module bundled with Redis 8.10 answers
//! them (`module.c`, `query_language.c`, `reply.c`, `resultset.c` and the parts of `tsdb.c`
//! that reach other keys).
//!
//! The module's conventions are kept:
//!
//! - `GetSeries`: an optional ACL check, then a key opened for reading (a counted lookup) or for
//!   writing (uncounted, the key signalled as modified whatever happens), then the type check;
//!   with `DeleteReferences` the links to vanished or relinked series are dropped first.
//! - The compaction work an upsert or a deletion does runs in the module's static context: the
//!   series it updates are reached in database 0 with the default user, whatever database the
//!   command runs in.
//! - Writes propagate only as the module replicates them (`RM_Replicate*`: one change), and the
//!   label index is global, keyed by series name, kept by the keyspace events it listens to
//!   ([`drain`]).

use super::bloom::{arg_index, double, nested_error};
use super::*;
use crate::acl::User;
use crate::cmd;
use crate::config::{TsCfg, ts_cfg};
use crate::engine::Mode;
use crate::notify;
use crate::ts::agg::AggType;
use crate::ts::chunk::{Dp, Sample};
use crate::ts::index::{self, Pred, c_str};
use crate::ts::iter::{self, Align, Batch, BucketTs, Chain, Latest, Range};
use crate::ts::{self, Create, OPT_COMPRESSED, OPT_UNCOMPRESSED, RStr, Rule, SKind, Series, bucket_start, normalize};
use crate::util::parse_i64;
use crate::value::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

pub static COMMANDS: &[Command] = &[
    cmd!("ts.create", -2, WRITE | DENYOOM | MODULE | ACL_FAST, 1, 1, 1, ts_create, Module),
    cmd!("ts.alter", -2, WRITE | DENYOOM | MODULE, 1, 1, 1, ts_alter, Module),
    cmd!("ts.createrule", -5, WRITE | MODULE | FAST, 1, 2, 1, ts_createrule, Module),
    cmd!("ts.deleterule", 3, WRITE | MODULE | ACL_FAST, 1, 2, 1, ts_deleterule, Module),
    cmd!("ts.add", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, ts_add, Module),
    cmd!("ts.incrby", -3, WRITE | DENYOOM | MODULE, 1, 1, 1, ts_incrby, Module),
    cmd!("ts.decrby", -3, WRITE | DENYOOM | MODULE, 1, 1, 1, ts_incrby, Module),
    cmd!("ts.range", -4, READONLY | MODULE, 1, 1, 1, ts_range, Module),
    cmd!("ts.revrange", -4, READONLY | MODULE, 1, 1, 1, ts_range, Module),
    cmd!("ts.queryindex", -2, READONLY | MODULE, 0, 0, 0, ts_queryindex, Module),
    cmd!("ts.querylabels", -2, READONLY | MODULE, 0, 0, 0, ts_querylabels, Module),
    cmd!("ts.info", -2, READONLY | MODULE | ACL_FAST, 1, 1, 1, ts_info, Module),
    cmd!("ts.get", -2, READONLY | MODULE | ACL_FAST, 1, 1, 1, ts_get, Module),
    cmd!("ts.read", -3, READONLY | MODULE, 1, 1, 1, ts_read, Module),
    cmd!("ts.del", 4, WRITE | MODULE, 1, 1, 1, ts_del, Module),
    cmd!("ts.madd", -4, WRITE | DENYOOM | MODULE, 1, -1, 3, ts_madd, Module),
    cmd!("ts.mrange", -4, READONLY | MODULE, 0, 0, 0, ts_mrange, Module),
    cmd!("ts.mrevrange", -4, READONLY | MODULE, 0, 0, 0, ts_mrange, Module),
    cmd!("ts.nrange", -5, READONLY | MODULE, keys = nrange_keys, ts_nrange, Module),
    cmd!("ts.nrevrange", -5, READONLY | MODULE, keys = nrange_keys, ts_nrange, Module),
    cmd!("ts.mget", -3, READONLY | MODULE, 0, 0, 0, ts_mget, Module),
    // LibMR's cluster topology commands (the module registers them on every deployment).
    cmd!("timeseries.CLUSTERSET", -1, READONLY | MODULE | NOSCRIPT, 0, 0, 0, ts_clusterset, Module),
    cmd!("timeseries.REFRESHCLUSTER", -1, READONLY | MODULE | NOSCRIPT, 0, 0, 0, ts_refreshcluster, Module),
];

/// `timeseries.CLUSTERSET`: with topology events on (always, outside a cluster) the short form
/// fails to read the cluster's nodes, and anything else doesn't parse.
fn ts_clusterset(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Err(Error::new(if args.len() == 1 { "ERRCLUSTER Failed to set cluster topology" } else { "Could not parse cluster set arguments" }))
}

/// `timeseries.REFRESHCLUSTER`: ignored when topology events are on.
fn ts_refreshcluster(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Ok)
}

/// TS.NRANGE's keys: `numkeys` of them after it (none when it doesn't parse or overruns).
fn nrange_keys(args: &[Bytes]) -> Vec<usize> {
    match args.get(1).and_then(|a| parse_i64(a)) {
        Some(n) if n > 0 && (n as u64) <= args.len().saturating_sub(2) as u64 => (2..2 + n as usize).collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Reaching series
// ---------------------------------------------------------------------------------------------

/// Whose context a lookup runs in: the command's (its database, its user), the module's static
/// context (`rts_staticCtx`: database 0, the default user) or a keyspace event's (the event's
/// database, no user: no ACL).
#[derive(Clone, Copy)]
enum By {
    Client,
    Static,
    Hook(usize),
}

impl By {
    fn db(self, cx: &Cx) -> usize {
        match self {
            By::Client => cx.client.db,
            By::Static => 0,
            By::Hook(db) => db,
        }
    }

    fn user(self, cx: &Cx) -> Option<Arc<User>> {
        match self {
            By::Client => Some(cx.client.user.clone()),
            By::Static => Some(cx.eng.acl.read().default_user()),
            By::Hook(_) => None,
        }
    }
}

/// `RM_OpenKey` modes.
const R: u8 = 1;
const W: u8 = 2;
/// `GetSeriesFlags`.
const REFS: u8 = 1;
const SILENT: u8 = 2;
const ACL: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Found {
    Empty,
    Other,
    Ts,
}

/// `RM_OpenKey` in database `db`: for writing an uncounted lookup that signals the key, for
/// reading a counted one.
fn open_key(cx: &mut Cx, db: usize, key: &[u8], write: bool) -> Found {
    let saved = (cx.client.db, cx.read_stats);
    cx.client.db = db;
    cx.read_stats = !write;
    let found = match cx.get_entry(key) {
        None => Found::Empty,
        Some(e) if matches!(e.value, Value::Ts(_)) => Found::Ts,
        Some(_) => Found::Other,
    };
    (cx.client.db, cx.read_stats) = saved;
    if write {
        cx.eng.sig.touch_quiet(db as u16, key);
    }
    found
}

/// `RM_KeyExists`: a counted lookup that leaves the access time alone.
fn key_exists(cx: &mut Cx, key: &[u8]) -> bool {
    let (saved, no_touch) = (cx.read_stats, cx.client.no_touch);
    cx.read_stats = true;
    cx.client.no_touch = true;
    let found = cx.get_entry(key).is_some();
    cx.read_stats = saved;
    cx.client.no_touch = no_touch;
    found
}

/// A series where it is, without any of a lookup's side effects.
fn peek<'a>(cx: &'a Cx, db: usize, key: &[u8]) -> Option<&'a Series> {
    match &cx.eng.dbs[db].keys.get(key)?.value {
        Value::Ts(s) => Some(s),
        _ => None,
    }
}

/// Change a series in place, keeping its database's memory count.
fn with<T>(cx: &mut Cx, db: usize, key: &[u8], f: impl FnOnce(&mut Series) -> T) -> Option<T> {
    let crate::db::Db { keys, mem, .. } = &mut cx.eng.dbs[db];
    let Value::Ts(s) = &mut keys.get_mut(key)?.value else { return None };
    let old = s.approx_mem();
    let r = f(s);
    *mem = (*mem + s.approx_mem()).saturating_sub(old);
    Some(r)
}

/// Why `GetSeries` failed, and what it would answer.
struct Fail {
    perm: bool,
    err: Error,
}

/// `GetSeries`: the series in `by`'s database.
fn get_series(cx: &mut Cx, by: By, key: &[u8], mut mode: u8, flags: u8) -> Result<(), Fail> {
    if flags & REFS != 0 {
        mode |= W;
    }
    if flags & ACL != 0
        && let Some(u) = by.user(cx)
    {
        if mode & R != 0 && !u.can_use_key(key, false) {
            let err = Error::new("NOPERM the current user doesn't have the read permission to one or more keys that match the specified filter");
            return Err(Fail { perm: true, err });
        }
        if mode & W != 0 && !u.can_use_key(key, true) {
            let err = Error::new("NOPERM the current user doesn't have the write permission to one or more keys that match the specified filter");
            return Err(Fail { perm: true, err });
        }
    }
    let db = by.db(cx);
    match open_key(cx, db, key, mode & W != 0) {
        Found::Empty => Err(Fail { perm: false, err: Error::new("ERR TSDB: the key does not exist") }),
        Found::Other => Err(Fail { perm: false, err: Error::new("ERR WRONGTYPE Operation against a key holding the wrong kind of value") }),
        Found::Ts => {
            if flags & REFS != 0 {
                delete_refs(cx, db, key, by, (flags & !REFS) | SILENT);
            }
            Ok(())
        }
    }
}

/// [`get_series`] answering its error.
fn series(cx: &mut Cx, key: &[u8], mode: u8, flags: u8) -> Result<(), Error> {
    get_series(cx, By::Client, key, mode, flags).map_err(|f| f.err)
}

/// `deleteReferenceToDeletedSeries`: forget a source that no longer has a rule to the series
/// (at `db`/`key`) and rules whose destination no longer names it as its source. The other
/// series are looked up in `by`'s context.
fn delete_refs(cx: &mut Cx, db: usize, key: &[u8], by: By, flags: u8) {
    let Some(s) = peek(cx, db, key) else { return };
    let name = s.key.b.clone();
    let src = s.src_key.as_ref().map(|k| k.b.clone());
    let dests: Vec<Vec<u8>> = s.rules.iter().map(|r| r.dest.b.clone()).collect();
    let at = by.db(cx);
    if let Some(src) = src {
        let ok = get_series(cx, by, &src, R, flags).is_ok() && peek(cx, at, &src).is_some_and(|x| x.rule(&name).is_some());
        if !ok {
            with(cx, db, key, |s| s.delete_src(&src));
        }
    }
    for d in dests {
        let ok = get_series(cx, by, &d, R, flags).is_ok() && peek(cx, at, &d).is_some_and(|x| x.src_key.as_ref().is_some_and(|k| k.b == name));
        if !ok {
            with(cx, db, key, |s| s.delete_rule(&d));
        }
    }
}

/// `RedisModule_SignalKeyAsReady` in a database.
fn ready(cx: &mut Cx, db: usize, key: &[u8]) {
    let saved = cx.client.db;
    cx.client.db = db;
    cx.signal_ready(key);
    cx.client.db = saved;
}

/// A module keyspace event in a database.
fn event(cx: &mut Cx, class: u32, name: &'static str, db: usize, key: &[u8]) {
    cx.eng.sig.notify(class, name, db as u16, key);
}

fn now_ms() -> i64 {
    crate::util::now_ms()
}

// ---------------------------------------------------------------------------------------------
// Samples and compactions
// ---------------------------------------------------------------------------------------------

/// What `internalAdd` did.
enum Added {
    /// Stored at this timestamp.
    Ok(u64),
    /// Ignored as too close to the last sample: answered with the last timestamp, not a write.
    Filtered(u64),
    Err(Error),
}

/// `internalAdd`: add or upsert a sample, feeding the compactions.
fn internal_add(cx: &mut Cx, by: By, db: usize, key: &[u8], ts: u64, v: f64, dp_override: Dp) -> Added {
    let Some(s) = peek(cx, db, key) else { return Added::Err(Error::new("ERR TSDB: the key does not exist")) };
    let (last_ts, retention) = (s.last_ts, s.retention);
    if retention != 0 && ts < last_ts && retention < last_ts - ts {
        return Added::Err(Error::new("ERR TSDB: Timestamp is older than retention"));
    }
    let global = ts_cfg(&cx.eng.cfg).dup;
    let dp = [dp_override, s.dup, global].into_iter().find(|d| *d != Dp::None).unwrap_or(Dp::None);
    // `filter_close_samples`.
    if !v.is_nan()
        && !s.last_value.is_nan()
        && dp == Dp::Last
        && s.total != 0
        && ts >= s.last_ts
        && ts - s.last_ts <= s.ignore_time as u64
        && (v - s.last_value).abs() <= s.ignore_val
    {
        return Added::Filtered(s.last_ts);
    }
    if ts <= s.last_ts && s.total != 0 {
        if series_upsert(cx, db, key, ts, v, dp).is_err() {
            return Added::Err(Error::new(
                "ERR TSDB: Error at upsert, update is not supported when DUPLICATE_POLICY is set to BLOCK mode, or either current or new value is NaN and DUPLICATE_POLICY is MAX/MIN/SUM",
            ));
        }
    } else {
        with(cx, db, key, |s| s.add_sample(ts, v));
        if peek(cx, db, key).is_some_and(|s| !s.rules.is_empty()) {
            delete_refs(cx, db, key, by, SILENT | ACL);
        }
        let n = peek(cx, db, key).map_or(0, |s| s.rules.len());
        for i in 0..n {
            handle_compaction(cx, by, db, key, i, ts, v);
        }
    }
    let name = peek(cx, db, key).map(|s| s.key.b.clone()).unwrap_or_else(|| key.to_vec());
    let at = by.db(cx);
    ready(cx, at, &name);
    Added::Ok(ts)
}

/// `handleCompaction`: a sample appended to a series feeds rule `i`; a new bucket closes the
/// previous one into the destination.
fn handle_compaction(cx: &mut Cx, by: By, db: usize, key: &[u8], i: usize, ts: u64, v: f64) {
    let Some(rule) = peek(cx, db, key).and_then(|s| s.rules.get(i)) else { return };
    let (bucket, align, kind) = (rule.bucket, rule.align, rule.agg.kind);
    let cur = bucket_start(ts, bucket, align);
    let curn = normalize(cur);
    with(cx, db, key, |s| {
        let r = &mut s.rules[i];
        if r.start == u64::MAX {
            r.start = curn;
            if kind.twa() {
                r.agg.bucket_params(curn, cur.wrapping_add(bucket));
            }
        }
    });
    let Some(rule) = peek(cx, db, key).map(|s| s.rules[i].clone()) else { return };
    if curn > rule.start {
        let dest = rule.dest.b.clone();
        if get_series(cx, by, &dest, R | W, SILENT | ACL).is_err() {
            return;
        }
        let at = by.db(cx);
        let mut r = rule;
        if kind.twa() && kind.valid(v) {
            r.agg.next_bucket_first(v, ts);
        }
        let had = r.valid;
        if had && let Ok(val) = r.agg.finalize() {
            internal_add(cx, by, at, &dest, r.start, val, Dp::Last);
            event(cx, notify::MODULE, "ts.add:dest", at, &dest);
        }
        let last = r.agg.last_sample();
        r.agg.reset();
        r.valid = false;
        if kind.twa() {
            r.agg.bucket_params(curn, cur.wrapping_add(bucket));
        }
        if kind.twa()
            && had
            && let Some((lts, lv)) = last
            && kind.valid(lv)
        {
            r.agg.prev_bucket_last(lv, lts);
        }
        r.start = curn;
        with(cx, db, key, |s| {
            if let Some(x) = s.rules.get_mut(i) {
                x.agg = r.agg;
                x.valid = r.valid;
                x.start = r.start;
            }
        });
    }
    if kind.valid(v) {
        with(cx, db, key, |s| {
            if let Some(r) = s.rules.get_mut(i) {
                r.agg.append(v, ts);
                r.valid = true;
            }
        });
    }
}

/// `SeriesCalcRange`: the aggregation of [start, end] and whether it saw no sample.
fn calc(cx: &Cx, db: usize, key: &[u8], start: u64, end: u64, kind: AggType) -> (ts::agg::Agg, bool) {
    match peek(cx, db, key) {
        Some(s) => s.calc_range(start, end, kind),
        None => (kind.create(false), true),
    }
}

/// [`calc`] into rule `i`'s context.
fn calc_into(cx: &mut Cx, db: usize, key: &[u8], i: usize, start: u64, end: u64, kind: AggType) -> bool {
    let (agg, empty) = calc(cx, db, key, start, end, kind);
    with(cx, db, key, |s| {
        if let Some(r) = s.rules.get_mut(i) {
            r.agg = agg;
        }
    });
    empty
}

/// [`calc`]'s finalized value (0 when empty).
fn calc_value(cx: &Cx, db: usize, key: &[u8], start: u64, end: u64, kind: AggType) -> (f64, bool) {
    let (agg, empty) = calc(cx, db, key, start, end, kind);
    (if empty { 0.0 } else { agg.value() }, empty)
}

/// `SeriesUpsertSample`.
fn series_upsert(cx: &mut Cx, db: usize, key: &[u8], ts: u64, v: f64, dp: Dp) -> Result<(), ts::chunk::Refused> {
    with(cx, db, key, |s| s.upsert_raw(ts, v, dp)).ok_or(ts::chunk::Refused)??;
    upsert_compaction(cx, db, key, ts);
    Ok(())
}

/// `upsertCompaction`: recompute the bucket the upserted sample falls in.
fn upsert_compaction(cx: &mut Cx, db: usize, key: &[u8], ts: u64) {
    if peek(cx, db, key).is_none_or(|s| s.rules.is_empty()) {
        return;
    }
    delete_refs(cx, db, key, By::Static, SILENT | ACL);
    let Some(s) = peek(cx, db, key) else { return };
    let last_ts = s.last_ts;
    let n = s.rules.len();
    for i in 0..n {
        let Some(r) = peek(cx, db, key).and_then(|s| s.rules.get(i)) else { break };
        let (bucket, align, kind, dest) = (r.bucket, r.align, r.agg.kind, r.dest.b.clone());
        let cur = bucket_start(last_ts, bucket, align);
        let curn = normalize(cur);
        if ts >= curn {
            calc_into(cx, db, key, i, curn, cur.wrapping_add(bucket).wrapping_sub(1), kind);
        } else {
            let start = bucket_start(ts, bucket, align);
            let sn = normalize(start);
            let (val, _) = calc_value(cx, db, key, sn, start.wrapping_add(bucket).wrapping_sub(1), kind);
            rule_upsert(cx, &dest, sn, val);
        }
    }
}

/// `RuleSeriesUpsertSample`: a compaction bucket rewritten (static context).
fn rule_upsert(cx: &mut Cx, dest: &[u8], start: u64, val: f64) -> bool {
    if get_series(cx, By::Static, dest, R | W, ACL).is_err() {
        return false;
    }
    if peek(cx, 0, dest).is_some_and(|d| d.total == 0) {
        with(cx, 0, dest, |d| d.add_sample(start, val));
    } else {
        let _ = series_upsert(cx, 0, dest, start, val, Dp::Last);
    }
    ready(cx, 0, dest);
    true
}

/// `SeriesDelRange`: the samples deleted.
fn series_del_range(cx: &mut Cx, db: usize, key: &[u8], start: u64, end: u64) -> u64 {
    let Some((deleted, before)) = with(cx, db, key, |s| s.del_range_raw(start, end)) else { return 0 };
    compaction_del_range(cx, db, key, start, end, before);
    deleted
}

/// `CompactionDelRange`: the buckets a deletion touched recomputed, emptied ones deleted.
fn compaction_del_range(cx: &mut Cx, db: usize, key: &[u8], start: u64, end: u64, before: u64) {
    if peek(cx, db, key).is_none_or(|s| s.rules.is_empty()) {
        return;
    }
    delete_refs(cx, db, key, By::Static, SILENT | ACL);
    let n = peek(cx, db, key).map_or(0, |s| s.rules.len());
    for i in 0..n {
        let Some(r) = peek(cx, db, key).and_then(|s| s.rules.get(i)) else { break };
        let (bucket, align, kind, dest) = (r.bucket, r.align, r.agg.kind, r.dest.b.clone());
        let cur = bucket_start(before, bucket, align);
        let curn = normalize(cur);
        let mut latest_deleted = false;
        if start >= curn {
            latest_deleted = calc_into(cx, db, key, i, curn, cur.wrapping_add(bucket).wrapping_sub(1), kind);
        } else {
            let sw = bucket_start(start, bucket, align);
            let swn = normalize(sw);
            let ew = bucket_start(end, bucket, align);
            let ewn = normalize(ew);
            let (val, empty) = calc_value(cx, db, key, swn, sw.wrapping_add(bucket).wrapping_sub(1), kind);
            let del_start = if empty {
                swn
            } else {
                if !rule_upsert(cx, &dest, swn, val) {
                    continue;
                }
                sw.wrapping_add(bucket)
            };
            let del_end;
            if end >= curn {
                latest_deleted = calc_into(cx, db, key, i, curn, u64::MAX, kind);
                del_end = normalize(ew.wrapping_sub(bucket));
            } else {
                let (val, empty) = calc_value(cx, db, key, ewn, ew.wrapping_add(bucket).wrapping_sub(1), kind);
                if empty {
                    del_end = ewn;
                } else {
                    del_end = normalize(ew.wrapping_sub(bucket));
                    if !rule_upsert(cx, &dest, ewn, val) {
                        continue;
                    }
                }
            }
            if del_end >= del_start && get_series(cx, By::Static, &dest, R | W, ACL).is_ok() {
                series_del_range(cx, 0, &dest, del_start, del_end);
            }
        }
        if latest_deleted {
            let last = delete_sample_before(cx, &dest, before).unwrap_or(0);
            let w = normalize(bucket_start(last, bucket, align));
            let empty = calc_into(cx, db, key, i, w, w.wrapping_add(bucket).wrapping_sub(1), kind);
            with(cx, db, key, |s| {
                if let Some(r) = s.rules.get_mut(i) {
                    r.start = if empty { u64::MAX } else { w };
                }
            });
        }
    }
}

/// `delete_sample_before`: delete the last sample of the destination's chunk before `ts`.
fn delete_sample_before(cx: &mut Cx, dest: &[u8], ts: u64) -> Option<u64> {
    get_series(cx, By::Static, dest, R | W, ACL).ok()?;
    let s = peek(cx, 0, dest)?;
    let c = s.dict.range(..ts).next_back().map(|(_, &id)| s.chunk(id))?;
    if c.num_samples() == 0 {
        return None;
    }
    let deleted = c.last_ts();
    series_del_range(cx, 0, dest, deleted, deleted);
    Some(deleted)
}

/// `calculate_latest_sample` for a compaction at `db`/`key`: its source's open bucket.
fn latest_of(cx: &mut Cx, db: usize, key: &[u8]) -> Option<Sample> {
    let s = peek(cx, db, key)?;
    let src = s.src_key.as_ref()?.b.clone();
    let name = s.key.b.clone();
    get_series(cx, By::Static, &src, R, SILENT | ACL).ok()?;
    open_bucket(peek(cx, 0, &src)?, &name)
}

fn open_bucket(src: &Series, name: &[u8]) -> Option<Sample> {
    if src.total == 0 {
        return None;
    }
    let r = src.rule(name)?;
    if r.start == u64::MAX {
        return None;
    }
    Some(Sample { ts: r.start, v: r.agg.value() })
}

/// The open bucket a range query with LATEST would reach, found without the lookup's side
/// effects (they happen in [`latest_used`] if the query asked for it).
fn latest_for(cx: &Cx, s: &Series, range: &Range) -> Option<Latest> {
    if !range.latest {
        return None;
    }
    let src = &s.src_key.as_ref()?.b;
    let user = cx.eng.acl.read().default_user();
    let now = cx.eng.expiry_clock(cx.now);
    let sample = if user.can_use_key(src, false) {
        match cx.eng.dbs[0].keys.get(src.as_slice()) {
            Some(e) if !e.expired(now) => match &e.value {
                Value::Ts(x) => open_bucket(x, &s.key.b),
                _ => None,
            },
            _ => None,
        }
    } else {
        None
    };
    Some(Latest { sample, used: Default::default() })
}

/// The lookup [`latest_for`] stood in for, if the query reached it.
fn latest_used(cx: &mut Cx, src: Option<Vec<u8>>, lat: &Option<Latest>) {
    if let (Some(src), Some(l)) = (src, lat)
        && l.used.get()
    {
        let _ = get_series(cx, By::Static, &src, R, SILENT | ACL);
    }
}

fn src_of(cx: &Cx, db: usize, key: &[u8]) -> Option<Vec<u8>> {
    peek(cx, db, key)?.src_key.as_ref().map(|k| k.b.clone())
}

// ---------------------------------------------------------------------------------------------
// Creating series
// ---------------------------------------------------------------------------------------------

/// `RM_CreateStringFromString`: a copy of a module string (a grown one comes out exact).
fn dup(s: &RStr) -> RStr {
    match s.kind {
        SKind::Printf => RStr::raw(&s.b),
        _ => s.clone(),
    }
}

/// `CopySeries`: the copy COPY makes (no source, no rules, named after the copy).
pub fn copy_series(s: &Series, to: &[u8]) -> Series {
    let mut c = s.copy_as(RStr::arg(to));
    for (k, v) in &mut c.labels {
        *k = dup(k);
        *v = dup(v);
    }
    c
}

/// `CreateTsKey`: store a new series (opened for writing already) and index it.
fn create_key(cx: &mut Cx, name: RStr, c: Create) {
    let key = name.b.clone();
    let s = Series::new(name, c, false);
    cx.eng.sig.ts_hook = true;
    let labels = s.labels.clone();
    cx.delete(&key);
    cx.set(&key, Value::Ts(Box::new(s)), false);
    cx.eng.ts.index.add(&key, &labels);
}

/// `SeriesCreateRulesFromGlobalConfig`: the compactions `ts-compaction-policy` asks for.
fn global_rules(cx: &mut Cx, key: &[u8], labels: &[(RStr, RStr)], cfg: &TsCfg) {
    let db = cx.client.db;
    for p in &cfg.rules {
        let agg = p.agg.upper();
        let mut dest = [c_str(key), b"_", agg.as_bytes(), b"_", p.bucket.to_string().as_bytes()].concat();
        if p.align != 0 {
            dest.extend_from_slice(format!("_{}", p.align).as_bytes());
        }
        if open_key(cx, db, &dest, true) != Found::Empty {
            continue;
        }
        let mut l: Vec<(RStr, RStr)> = labels.iter().map(|(k, v)| (dup(k), dup(v))).collect();
        l.push((RStr::printf(b"aggregation"), RStr::arg(agg.as_bytes())));
        l.push((RStr::printf(b"time_bucket"), RStr::printf(p.bucket.to_string().as_bytes())));
        let c = Create {
            retention: p.retention,
            chunk_size: cfg.chunk_size,
            labels: Some(l),
            options: if cfg.uncompressed { OPT_UNCOMPRESSED } else { 0 },
            dup: Dp::None,
            ignore_time: 0,
            ignore_val: 0.0,
        };
        create_key(cx, RStr::printf(&dest), c);
        let Some(src_name) = peek(cx, db, key).map(|s| s.key.clone()) else { return };
        let Some(dest_name) = peek(cx, db, &dest).map(|s| s.key.clone()) else { continue };
        with(cx, db, &dest, |d| d.src_key = Some(src_name));
        with(cx, db, key, |s| s.rules.push(Rule::new(dest_name, p.agg, p.bucket, p.align)));
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

/// `RMUtil_ParseArgsAfter`'s reach: the `n` arguments after `token`.
fn after<'a>(args: &'a [Bytes], token: &str, n: usize) -> Option<&'a [Bytes]> {
    let p = arg_index(args, token)?;
    args.get(p + 1..p + 1 + n)
}

/// `arg_index` past the command name (the module's `> 0` checks).
fn opt(args: &[Bytes], token: &str) -> Option<usize> {
    arg_index(args, token).filter(|&i| i > 0)
}

/// `parseTimestamp`.
fn timestamp(b: &[u8]) -> Option<u64> {
    parse_i64(b).filter(|&v| v >= 0).map(|v| v as u64)
}

/// `strcasecmp` against the bytes before a NUL.
fn c_eq(b: &[u8], s: &str) -> bool {
    c_str(b).eq_ignore_ascii_case(s.as_bytes())
}

/// `parseCreateArgs` over the whole command.
fn parse_create(cx: &Cx, args: &[Bytes], cfg: &TsCfg) -> Result<Create, Error> {
    let mut c = Create {
        retention: cfg.retention,
        chunk_size: cfg.chunk_size,
        labels: None,
        options: 0,
        dup: cfg.dup,
        ignore_time: cfg.ignore_time,
        ignore_val: cfg.ignore_val,
    };
    if let Some(pos) = arg_index(args, "LABELS") {
        let n = (args.len() - pos - 1) / 2;
        if n > 0 {
            let mut labels = Vec::with_capacity(n);
            for i in 0..n {
                let (k, v) = (&args[pos + 1 + 2 * i], &args[pos + 2 + 2 * i]);
                if k.is_empty() || v.is_empty() || c_str(v).iter().any(|b| b"(),".contains(b)) {
                    return Err(Error::new("ERR TSDB: Couldn't parse LABELS"));
                }
                labels.push((RStr::arg(k), RStr::arg(v)));
            }
            c.labels = Some(labels);
        }
    }
    if opt(args, "RETENTION").is_some() {
        let v = after(args, "RETENTION", 1).and_then(|a| parse_i64(&a[0])).ok_or(Error::new("ERR TSDB: Couldn't parse RETENTION"))?;
        if v < 0 {
            return Err(Error::new("TSDB: Couldn't parse RETENTION"));
        }
        c.retention = v as u64;
    }
    if arg_index(args, "CHUNK_SIZE").is_some() {
        let v = after(args, "CHUNK_SIZE", 1).and_then(|a| parse_i64(&a[0])).ok_or(Error::new("ERR TSDB: Couldn't parse CHUNK_SIZE"))?;
        if !(48..=1_048_576).contains(&v) || v % 8 != 0 {
            return Err(Error::new("ERR TSDB: CHUNK_SIZE value must be a multiple of 8 in the range [48 .. 1048576]"));
        }
        c.chunk_size = v as u64;
    }
    let set = |o: &mut u64, uncompressed: bool| *o = (*o & !OPT_COMPRESSED) | if uncompressed { OPT_UNCOMPRESSED } else { OPT_COMPRESSED };
    match opt(args, "ENCODING") {
        Some(i) => {
            let Some(e) = args.get(i + 1) else { return Err(arity_error(cx.cmd.name)) };
            if c_eq(e, "uncompressed") {
                set(&mut c.options, true);
            } else if c_eq(e, "compressed") {
                set(&mut c.options, false);
            } else {
                return Err(Error::new("ERR TSDB: unknown ENCODING parameter"));
            }
        }
        None => {
            if opt(args, "UNCOMPRESSED").is_some() {
                set(&mut c.options, true);
            }
            if opt(args, "COMPRESSED").is_some() {
                set(&mut c.options, false);
            }
        }
    }
    if let Some(dp) = parse_dp(args, "DUPLICATE_POLICY")? {
        c.dup = dp;
    }
    if let Some(i) = opt(args, "IGNORE") {
        let parsed = (i + 2 < args.len()).then(|| (parse_i64(&args[i + 1]), double(&args[i + 2])));
        let Some((Some(t), Some(v))) = parsed else { return Err(Error::new("ERR TSDB: Couldn't parse IGNORE")) };
        if t < 0 || v < 0.0 {
            return Err(Error::new("ERR TSDB: IGNORE arguments cannot be negative"));
        }
        c.ignore_time = t;
        c.ignore_val = v;
    }
    Ok(c)
}

/// `ParseDuplicatePolicy` after `token`.
fn parse_dp(args: &[Bytes], token: &str) -> Result<Option<Dp>, Error> {
    if arg_index(args, token).is_none() {
        return Ok(None);
    }
    let v = after(args, token, 1).ok_or(Error::new("ERR TSDB: Couldn't parse DUPLICATE_POLICY"))?;
    Dp::parse(&v[0]).map(Some).ok_or(Error::new("ERR TSDB: Unknown DUPLICATE_POLICY"))
}

/// `ParseAggSpec`: comma-separated aggregation types.
fn parse_agg_spec(spec: &[u8]) -> Result<Vec<AggType>, Error> {
    let mut out = Vec::new();
    for tok in spec.split(|&c| c == b',') {
        if tok.is_empty() {
            return Err(Error::new("ERR TSDB: Empty aggregation type in list"));
        }
        if out.len() >= 16 {
            return Err(Error::new("ERR TSDB: Too many aggregation types"));
        }
        out.push(AggType::parse(tok).ok_or(Error::new("ERR TSDB: Unknown aggregation type"))?);
    }
    Ok(out)
}

/// What `_parseAggregationArgs` found.
struct AggArgs {
    types: Vec<AggType>,
    bucket: u64,
    empty: bool,
    bucket_ts: BucketTs,
    align: u64,
}

/// `_parseAggregationArgs`: `None` without AGGREGATION. A range takes EMPTY and
/// BUCKETTIMESTAMP, a rule an alignment.
fn parse_agg(cx: &Cx, o: &[Bytes], range: bool) -> Result<Option<AggArgs>, Error> {
    let Some(off) = opt(o, "AGGREGATION") else { return Ok(None) };
    let parse_err = Error::new("ERR TSDB: Couldn't parse AGGREGATION");
    let (Some(spec), Some(b)) = (o.get(off + 1), o.get(off + 2)) else { return Err(parse_err) };
    let bucket = parse_i64(b).ok_or(parse_err)?;
    let types = parse_agg_spec(spec)?;
    if bucket <= 0 {
        return Err(Error::new("ERR TSDB: bucketDuration must be greater than zero"));
    }
    let mut a = AggArgs { types, bucket: bucket as u64, empty: false, bucket_ts: BucketTs::Start, align: 0 };
    if range {
        if let Some(e) = opt(o, "EMPTY") {
            if e != off + 3 && e != off + 5 {
                return Err(Error::new("ERR TSDB: EMPTY flag should be the 3rd or 5th flag after AGGREGATION flag"));
            }
            a.empty = true;
        }
        if let Some(p) = opt(o, "BUCKETTIMESTAMP") {
            if p != off + 3 && p != off + 4 {
                return Err(Error::new("ERR TSDB: BUCKETTIMESTAMP flag should be the 3rd or 4th flag after AGGREGATION flag"));
            }
            let Some(v) = o.get(p + 1) else { return Err(arity_error(cx.cmd.name)) };
            a.bucket_ts = if c_eq(v, "start") || c_eq(v, "-") {
                BucketTs::Start
            } else if c_eq(v, "end") || c_eq(v, "+") {
                BucketTs::End
            } else if c_eq(v, "mid") || c_eq(v, "~") {
                BucketTs::Mid
            } else {
                return Err(Error::new("ERR TSDB: unknown BUCKETTIMESTAMP parameter"));
            };
        }
    } else if o.len() == 7 {
        a.align = o.get(off + 3).and_then(|v| timestamp(v)).ok_or(Error::new("ERR TSDB: Couldn't parse alignTimestamp"))?;
    }
    Ok(Some(a))
}

/// `parseCountArgument`.
fn parse_count(o: &[Bytes]) -> Result<i64, Error> {
    let Some(mut off) = opt(o, "COUNT") else { return Ok(-1) };
    let rest = |from: usize| o.get(from..).unwrap_or(&[]);
    if let Some(a) = opt(o, "AGGREGATION")
        && off == a + 1
    {
        match arg_index(rest(a + 2), "COUNT") {
            None => return Ok(-1),
            Some(x) => off = x + a + 2,
        }
    }
    if let Some(r) = opt(o, "REDUCE")
        && off == r + 1
    {
        match arg_index(rest(r + 2), "COUNT") {
            None => return Ok(-1),
            Some(x) => off = x + r + 2,
        }
    }
    if off + 1 == o.len() {
        return Err(Error::new("ERR TSDB: COUNT argument is missing"));
    }
    let n = parse_i64(&o[off + 1]).ok_or(Error::new("ERR TSDB: Couldn't parse COUNT"))?;
    if n < 1 {
        return Err(Error::new("ERR TSDB: Invalid COUNT value"));
    }
    Ok(n)
}

/// `parseRangeArguments`: from/to at `start`, options scanned from there.
fn parse_range(cx: &Cx, args: &[Bytes], start: usize) -> Result<Range, Error> {
    let mut r = Range::default();
    let min = c_str(&args[start]) == b"-";
    if !min {
        r.start = timestamp(&args[start]).ok_or(Error::new("ERR TSDB: wrong fromTimestamp"))?;
    }
    let max = c_str(&args[start + 1]) == b"+";
    r.end = if max { i64::MAX as u64 } else { timestamp(&args[start + 1]).ok_or(Error::new("ERR TSDB: wrong toTimestamp"))? };
    let o = &args[start..];
    r.latest = opt(o, "LATEST").is_some();
    r.count = parse_count(o)?;
    if let Some(a) = parse_agg(cx, o, true)? {
        r.aggs = a.types;
        r.bucket = a.bucket;
        r.empty = a.empty;
        r.bucket_ts = a.bucket_ts;
    }
    if let Some(p) = opt(o, "ALIGN") {
        let Some(v) = o.get(p + 1) else { return Err(arity_error(cx.cmd.name)) };
        if c_eq(v, "start") || c_eq(v, "-") {
            r.align = Align::Start;
        } else if c_eq(v, "end") || c_eq(v, "+") {
            r.align = Align::End;
        } else if let Some(t) = timestamp(v) {
            r.align = Align::Ts;
            r.align_ts = t;
        } else {
            return Err(Error::new("ERR TSDB: unknown ALIGN parameter"));
        }
    }
    if r.align != Align::Default {
        if r.aggs.is_empty() {
            return Err(Error::new("ERR TSDB: ALIGN parameter can only be used with AGGREGATION"));
        }
        if r.align == Align::Start && min {
            return Err(Error::new("ERR TSDB: start alignment can only be used with explicit start timestamp"));
        }
        if r.align == Align::End && max {
            return Err(Error::new("ERR TSDB: end alignment can only be used with explicit end timestamp"));
        }
    }
    if let Some(p) = opt(o, "FILTER_BY_VALUE") {
        if p + 2 >= o.len() {
            return Err(Error::new("ERR TSDB: FILTER_BY_VALUE one or more arguments are missing"));
        }
        let lo = double(&o[p + 1]).ok_or(Error::new("ERR TSDB: Couldn't parse MIN"))?;
        let hi = double(&o[p + 2]).ok_or(Error::new("ERR TSDB: Couldn't parse MAX"))?;
        r.by_value = Some((lo, hi));
    }
    if let Some(mut p) = opt(o, "FILTER_BY_TS") {
        let missing = Error::new("ERR TSDB: FILTER_BY_TS one or more arguments are missing");
        if p + 1 == o.len() {
            return Err(missing);
        }
        let mut v = Vec::new();
        while p + 1 < o.len() && v.len() < 128 {
            match timestamp(&o[p + 1]) {
                Some(t) => {
                    v.push(t);
                    p += 1;
                }
                None => break,
            }
        }
        if v.is_empty() {
            return Err(missing);
        }
        v.sort_unstable();
        v.dedup();
        r.by_ts = Some(v);
    }
    Ok(r)
}

/// `parseFilter`: the filter expressions.
fn parse_filter(exprs: &[Bytes]) -> Result<Vec<Pred>, Error> {
    let p = index::parse_filters(exprs).ok_or(Error::new("ERR TSDB: failed parsing labels"))?;
    if !index::has_matcher(&p) {
        return Err(Error::new("ERR TSDB: please provide at least one matcher"));
    }
    Ok(p)
}

const QUERY_TOKENS: [&str; 10] = ["WITHLABELS", "AGGREGATION", "LIMIT", "GROUPBY", "REDUCE", "FILTER", "FILTER_BY_VALUE", "FILTER_BY_TS", "COUNT", "EXCLUDEEMPTY"];

/// `parseLabelQuery`: WITHLABELS, or the labels SELECTED_LABELS names.
fn parse_label_query(args: &[Bytes]) -> Result<(bool, Vec<Bytes>), Error> {
    let with = opt(args, "WITHLABELS").is_some();
    let Some(p) = opt(args, "SELECTED_LABELS") else { return Ok((with, Vec::new())) };
    if with {
        return Err(Error::new("ERR TSDB: cannot accept WITHLABELS and SELECT_LABELS together"));
    }
    let mut out = Vec::new();
    for a in &args[p + 1..] {
        if QUERY_TOKENS.iter().any(|t| c_eq(a, t)) {
            break;
        }
        if out.len() >= 50 {
            return Err(Error::new("ERR TSDB: reached max size for SELECT_LABELS"));
        }
        out.push(a.clone());
    }
    if out.is_empty() {
        return Err(Error::new("ERR TSDB: SELECT_LABELS should have at least 1 parameter"));
    }
    Ok((false, out))
}

/// The series matching a filter, checked for read permission (`QueryIndex`); an error when
/// one is denied.
fn query_readable(cx: &mut Cx, preds: &mut [Pred]) -> Result<Vec<Vec<u8>>, Error> {
    let user = cx.client.user.clone();
    let mut can = |k: &[u8]| user.can_use_key(k, false);
    let (keys, denied) = cx.eng.ts.index.query(preds, Some(&mut can));
    if denied {
        return Err(perm_error());
    }
    Ok(keys.into_iter().collect())
}

fn perm_error() -> Error {
    Error::new("NOPERM TSDB: current user doesn't have read permission to one or more keys that match the specified filter")
}

/// `CheckDictSeriesPermissions`: every series readable, each looked up once.
fn check_permissions(cx: &mut Cx, keys: &[Vec<u8>]) -> Result<(), Error> {
    let user = cx.client.user.clone();
    for k in keys {
        if !user.can_use_key(k, false) {
            return Err(perm_error());
        }
        let _ = get_series(cx, By::Client, k, R, SILENT);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------------------------

fn st(s: impl Into<String>) -> Reply {
    Reply::Status(s.into())
}

/// `ReplyWithDoubleOrString`.
fn value(resp3: bool, v: f64) -> Reply {
    if resp3 { Reply::Double(v) } else { Reply::Status(ts::fmt::double(v)) }
}

fn sample(resp3: bool, ts: u64, v: f64) -> Reply {
    Reply::Array(vec![Reply::Int(ts as i64), value(resp3, v)])
}

/// `ReplyWithSeriesLastDatapoint`.
fn last_point(resp3: bool, s: &Series) -> Reply {
    if s.total == 0 { Reply::Array(Vec::new()) } else { sample(resp3, s.last_ts, s.last_value) }
}

/// A map in RESP3, an array of pairs otherwise.
fn pairs(resp3: bool, items: Vec<(Reply, Reply)>) -> Reply {
    if resp3 { Reply::Map(items) } else { Reply::Array(items.into_iter().map(|(k, v)| Reply::Array(vec![k, v])).collect()) }
}

/// `ReplyWithSeriesLabels`.
fn labels_reply(resp3: bool, labels: &[(RStr, RStr)]) -> Reply {
    pairs(resp3, labels.iter().map(|(k, v)| (Reply::bulk(&k.b), Reply::bulk(&v.b))).collect())
}

/// `ReplyWithSeriesLabelsWithLimit`: the selected labels (case-insensitively), null when absent.
fn selected_reply(resp3: bool, labels: &[(RStr, RStr)], select: &[Bytes]) -> Reply {
    let items = select
        .iter()
        .map(|sel| match labels.iter().find(|(k, _)| c_str(&k.b).eq_ignore_ascii_case(c_str(sel))) {
            Some((k, v)) => (Reply::bulk(&k.b), Reply::bulk(&v.b)),
            None => (Reply::bulk(c_str(sel)), Reply::Nil),
        })
        .collect();
    pairs(resp3, items)
}

fn series_labels(resp3: bool, labels: &[(RStr, RStr)], with: bool, select: &[Bytes]) -> Reply {
    if with {
        labels_reply(resp3, labels)
    } else if !select.is_empty() {
        selected_reply(resp3, labels, select)
    } else {
        pairs(resp3, Vec::new())
    }
}

/// `ReplySeriesRangeFromIter`: at most `count` samples (-1: all), the first batch possibly
/// taken already.
fn samples_reply(resp3: bool, mut chain: Box<dyn Chain + '_>, mut first: Option<Batch>, count: i64) -> Reply {
    let limit = if count != -1 { count as usize } else { usize::MAX };
    let mut out = Vec::new();
    while out.len() < limit {
        let Some(b) = first.take().or_else(|| chain.next()) else { break };
        let n = (limit - out.len()).min(b.len());
        for i in 0..n {
            if b.vps > 1 {
                let mut row = Vec::with_capacity(b.vps + 1);
                row.push(Reply::Int(b.ts[i] as i64));
                row.extend(b.row(i).iter().map(|&v| value(resp3, v)));
                out.push(Reply::Array(row));
            } else {
                out.push(sample(resp3, b.ts[i], b.v[i * b.vps.max(1)]));
            }
        }
    }
    Reply::Array(out)
}

// ---------------------------------------------------------------------------------------------
// Keyspace events
// ---------------------------------------------------------------------------------------------

/// The module's keyspace event subscription (`NotifyCallback`): the events queued since the
/// last call, handled in order.
pub fn drain(cx: &mut Cx) {
    while !cx.eng.sig.ts_events.is_empty() {
        for (ev, db, key) in std::mem::take(&mut cx.eng.sig.ts_events) {
            let db = db as usize;
            match ev {
                "restore" => restore_key(cx, db, &key),
                "rename_from" => cx.eng.ts.rename_from = Some(key.to_vec()),
                "rename_to" => rename_to(cx, db, &key),
                _ => cx.eng.ts.index.remove(&key),
            }
        }
    }
}

/// `RestoreKey`: a restored series takes its key's name and is indexed.
fn restore_key(cx: &mut Cx, db: usize, key: &[u8]) {
    if get_series(cx, By::Hook(db), key, R | W, SILENT).is_err() {
        return;
    }
    let Some(labels) = with(cx, db, key, |s| {
        s.key = RStr::arg(key);
        s.labels.clone()
    }) else {
        return;
    };
    if cx.eng.ts.index.is_indexed(key) {
        cx.eng.ts.index.remove(key);
    }
    cx.eng.ts.index.add(key, &labels);
}

/// `RenameSeriesTo`: a renamed series reindexed, its links renamed on the other side.
fn rename_to(cx: &mut Cx, db: usize, to: &[u8]) {
    let from = cx.eng.ts.rename_from.take().unwrap_or_default();
    let by = By::Hook(db);
    if get_series(cx, by, to, R | W, REFS | SILENT | ACL).is_err() {
        return;
    }
    let Some(s) = peek(cx, db, to) else { return };
    let labels = s.labels.clone();
    let src = s.src_key.as_ref().map(|k| k.b.clone());
    let dests: Vec<Vec<u8>> = s.rules.iter().map(|r| r.dest.b.clone()).collect();
    cx.eng.ts.index.remove(&from);
    cx.eng.ts.index.add(to, &labels);
    if let Some(src) = src
        && get_series(cx, by, &src, W, ACL).is_ok()
    {
        with(cx, db, &src, |x| {
            if let Some(r) = x.rules.iter_mut().find(|r| r.dest.b == from) {
                r.dest = RStr::arg(to);
            }
        });
    }
    for d in dests {
        if get_series(cx, by, &d, W, ACL).is_ok() {
            with(cx, db, &d, |x| x.src_key = Some(RStr::arg(to)));
        }
    }
    with(cx, db, to, |s| s.key = RStr::arg(to));
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// TS.CREATE key [RETENTION ms] [ENCODING enc] [CHUNK_SIZE n] [DUPLICATE_POLICY p] [IGNORE t v]
/// [LABELS l v ...]
fn ts_create(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    cx.changes(0);
    let cfg = ts_cfg(&cx.eng.cfg);
    let c = parse_create(cx, args, &cfg)?;
    let key = &args[1];
    let db = cx.client.db;
    if open_key(cx, db, key, true) != Found::Empty {
        return Err(Error::new("ERR TSDB: key already exists"));
    }
    create_key(cx, RStr::arg(key), c);
    cx.changes(1);
    event(cx, notify::MODULE, "ts.create", db, key);
    Ok(Reply::Ok)
}

/// TS.ALTER key [options as TS.CREATE]
fn ts_alter(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    cx.changes(0);
    let cfg = ts_cfg(&cx.eng.cfg);
    let c = parse_create(cx, args, &cfg)?;
    let key = &args[1];
    series(cx, key, R | W, 0)?;
    let db = cx.client.db;
    let has = |t: &str| opt(args, t).is_some();
    let (retention, chunk, dp, labels, ignore) = (has("RETENTION"), has("CHUNK_SIZE"), has("DUPLICATE_POLICY"), has("LABELS"), has("IGNORE"));
    with(cx, db, key, |s| {
        if retention {
            s.retention = c.retention;
        }
        if chunk {
            s.chunk_size = c.chunk_size;
        }
        if dp {
            s.dup = c.dup;
        }
        if ignore {
            s.ignore_time = c.ignore_time;
            s.ignore_val = c.ignore_val;
        }
    });
    if labels {
        cx.eng.ts.index.remove(key);
        let l = with(cx, db, key, |s| {
            s.labels_alloc = c.labels.is_some();
            s.labels = c.labels.clone().unwrap_or_default();
            s.labels.clone()
        })
        .unwrap_or_default();
        cx.eng.ts.index.add(key, &l);
    }
    cx.changes(1);
    event(cx, notify::MODULE, "ts.alter", db, key);
    Ok(Reply::Ok)
}

/// TS.CREATERULE src dest AGGREGATION type bucket [align]
fn ts_createrule(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    cx.changes(0);
    if args.len() != 6 && args.len() != 7 {
        return Err(arity_error(cx.cmd.name));
    }
    let Some(a) = parse_agg(cx, args, false)? else { return Err(arity_error(cx.cmd.name)) };
    if a.types.len() != 1 {
        return Err(Error::new("ERR TSDB: CREATERULE requires exactly one aggregation type"));
    }
    let (src, dest) = (&args[1], &args[2]);
    if src == dest {
        return Err(Error::new("ERR TSDB: the source key and destination key should be different"));
    }
    let db = cx.client.db;
    series(cx, src, R | W, REFS | ACL)?;
    if peek(cx, db, src).is_some_and(|s| s.src_key.is_some()) {
        return Err(Error::new("ERR TSDB: the source key already has a source rule"));
    }
    series(cx, dest, R | W, REFS | ACL)?;
    let (Some(s), Some(d)) = (peek(cx, db, src), peek(cx, db, dest)) else { return Err(Error::new("ERR TSDB: the key does not exist")) };
    if !d.rules.is_empty() {
        return Err(Error::new("ERR TSDB: the destination key already has a dst rule"));
    }
    if d.src_key.is_some() {
        return Err(Error::new("ERR TSDB: the destination key already has a src rule"));
    }
    let (src_name, dest_name) = (s.key.clone(), d.key.clone());
    with(cx, db, dest, |d| d.src_key = Some(src_name));
    with(cx, db, src, |s| s.rules.push(Rule::new(dest_name, a.types[0], a.bucket, a.align)));
    cx.changes(1);
    event(cx, notify::MODULE, "ts.createrule:src", db, src);
    event(cx, notify::MODULE, "ts.createrule:dest", db, dest);
    Ok(Reply::Ok)
}

/// TS.DELETERULE src dest
fn ts_deleterule(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    cx.changes(0);
    let (src, dest) = (&args[1], &args[2]);
    let db = cx.client.db;
    series(cx, src, R | W, REFS)?;
    if !with(cx, db, src, |s| s.delete_rule(dest)).unwrap_or(false) {
        return Err(Error::new("ERR TSDB: compaction rule does not exist"));
    }
    series(cx, dest, R | W, REFS)?;
    with(cx, db, dest, |d| d.delete_src(src));
    cx.changes(1);
    event(cx, notify::MODULE, "ts.deleterule:src", db, src);
    event(cx, notify::MODULE, "ts.deleterule:dest", db, dest);
    Ok(Reply::Ok)
}

/// `add`: one sample, creating the series from the command's options if `create` is given.
fn add(cx: &mut Cx, key: &[u8], ts: &[u8], val: &[u8], create: Option<&[Bytes]>) -> Added {
    let db = cx.client.db;
    let found = open_key(cx, db, key, true);
    let Some(v) = ts::fmt::parse_value(val) else { return Added::Err(Error::new("ERR TSDB: invalid value")) };
    let Some(t) = parse_i64(ts) else { return Added::Err(Error::new("ERR TSDB: invalid timestamp")) };
    if t < 0 {
        return Added::Err(Error::new("ERR TSDB: invalid timestamp, must be a nonnegative integer"));
    }
    let mut dp = Dp::None;
    match (create, found) {
        (Some(args), Found::Empty) => {
            let cfg = ts_cfg(&cx.eng.cfg);
            let c = match parse_create(cx, args, &cfg) {
                Ok(c) => c,
                Err(e) => return Added::Err(e),
            };
            let labels = c.labels.clone().unwrap_or_default();
            create_key(cx, RStr::arg(key), c);
            global_rules(cx, key, &labels, &cfg);
        }
        (_, Found::Empty | Found::Other) => return Added::Err(Error::new("ERR TSDB: the key is not a TSDB key")),
        (Some(args), Found::Ts) => match parse_dp(args, "ON_DUPLICATE") {
            Ok(d) => dp = d.unwrap_or(Dp::None),
            Err(e) => return Added::Err(e),
        },
        (None, Found::Ts) => {}
    }
    internal_add(cx, By::Client, db, key, t as u64, v, dp)
}

/// TS.ADD key timestamp value [options as TS.CREATE] [ON_DUPLICATE policy]
fn ts_add(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    cx.changes(0);
    let key = &args[1];
    let ts = if args[2].as_ref() == b"*" { Bytes::from(now_ms().to_string()) } else { args[2].clone() };
    let r = add(cx, key, &ts, &args[3], Some(args));
    if matches!(r, Added::Ok(_)) {
        let mut repl = args.to_vec();
        repl[0] = Bytes::from_static(b"TS.ADD");
        repl[2] = ts;
        cx.changes(1);
        cx.propagate_as(repl);
    }
    let db = cx.client.db;
    event(cx, notify::MODULE, "ts.add", db, key);
    match r {
        Added::Ok(t) | Added::Filtered(t) => Ok(Reply::Int(t as i64)),
        Added::Err(e) => Err(e),
    }
}

/// TS.MADD key timestamp value [key timestamp value ...]
fn ts_madd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    cx.changes(0);
    if !(args.len() - 1).is_multiple_of(3) {
        return Err(arity_error(cx.cmd.name));
    }
    let mut now = None;
    let mut out = Vec::with_capacity(args.len() / 3);
    let mut repl = vec![Bytes::from_static(b"TS.MADD")];
    for t in args[1..].chunks(3) {
        let ts = if t[1].as_ref() == b"*" { now.get_or_insert_with(|| Bytes::from(now_ms().to_string())).clone() } else { t[1].clone() };
        match add(cx, &t[0], &ts, &t[2], None) {
            Added::Ok(v) => {
                out.push(Reply::Int(v as i64));
                repl.extend([t[0].clone(), ts, t[2].clone()]);
            }
            Added::Filtered(v) => out.push(Reply::Int(v as i64)),
            Added::Err(e) => out.push(nested_error(cx, &e.0)),
        }
    }
    if repl.len() > 1 {
        cx.changes(1);
        cx.propagate_as(repl);
    }
    let db = cx.client.db;
    for t in args[1..].chunks(3) {
        event(cx, notify::MODULE, "ts.add", db, &t[0]);
    }
    Ok(Reply::Array(out))
}

/// TS.INCRBY / TS.DECRBY key value [TIMESTAMP ts] [options as TS.CREATE]
fn ts_incrby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    cx.changes(0);
    let key = &args[1];
    let db = cx.client.db;
    let found = open_key(cx, db, key, true);
    if found == Found::Other {
        return Err(Error::new("ERR WRONGTYPE Operation against a key holding the wrong kind of value"));
    }
    let incr = double(&args[2]).ok_or(Error::new("ERR TSDB: invalid increase/decrease value"))?;
    let o = &args[3..];
    let mut tl = arg_index(o, "TIMESTAMP").map(|i| i + 3);
    let ll = arg_index(o, "LABELS").map(|i| i + 3);
    if let (Some(l), Some(t)) = (ll, tl)
        && t > l
    {
        tl = None;
    }
    if tl.is_some_and(|t| t + 1 >= args.len()) {
        return Err(arity_error(cx.cmd.name));
    }
    let local = tl.is_none_or(|t| args[t + 1].as_ref() == b"*");
    let cur = if local { now_ms() } else { parse_i64(&args[tl.expect("given") + 1]).ok_or(Error::new("ERR TSDB: invalid timestamp"))? };
    if found == Found::Empty {
        let cfg = ts_cfg(&cx.eng.cfg);
        let c = parse_create(cx, args, &cfg)?;
        let labels = c.labels.clone().unwrap_or_default();
        create_key(cx, RStr::arg(key), c);
        global_rules(cx, key, &labels, &cfg);
    }
    let Some(s) = peek(cx, db, key) else { return Err(Error::new("ERR TSDB: the key does not exist")) };
    if (cur as u64) < s.last_ts && s.last_ts != 0 {
        return Err(Error::new("TSDB: timestamp must be equal to or higher than the maximum existing timestamp"));
    }
    if s.last_value.is_nan() {
        return Err(Error::new("ERR TSDB: cannot increment/decrement NaN value"));
    }
    let is_incr = cx.cmd.name == "ts.incrby";
    let result = if is_incr { s.last_value + incr } else { s.last_value - incr };
    let r = internal_add(cx, By::Client, db, key, cur as u64, result, Dp::Last);
    if matches!(r, Added::Ok(_)) {
        let name = Bytes::from_static(if is_incr { b"TS.INCRBY" } else { b"TS.DECRBY" });
        let cur_b = Bytes::from(cur.to_string());
        let repl: Vec<Bytes> = match (local, tl, ll) {
            (true, None, Some(l)) => [&[name][..], &args[1..l], &[Bytes::from_static(b"TIMESTAMP"), cur_b], &args[l..]].concat(),
            (true, None, None) => [&[name][..], &args[1..], &[Bytes::from_static(b"TIMESTAMP"), cur_b]].concat(),
            (true, Some(t), _) => [&[name][..], &args[1..=t], &[cur_b], &args[t + 2..]].concat(),
            (false, ..) => {
                let mut v = args.to_vec();
                v[0] = Bytes::from(args[0].to_ascii_lowercase());
                v
            }
        };
        cx.changes(1);
        cx.propagate_as(repl);
    }
    event(cx, notify::GENERIC, if is_incr { "ts.incrby" } else { "ts.decrby" }, db, key);
    match r {
        Added::Ok(t) | Added::Filtered(t) => Ok(Reply::Int(t as i64)),
        Added::Err(e) => Err(e),
    }
}

/// TS.DEL key from to
fn ts_del(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    cx.changes(0);
    let r = parse_range(cx, args, 2)?;
    let key = &args[1];
    series(cx, key, R | W, 0)?;
    let db = cx.client.db;
    let Some(s) = peek(cx, db, key) else { return Err(Error::new("ERR TSDB: the key does not exist")) };
    if !s.rules.is_empty() && s.retention != 0 {
        let obsolete = |t: u64| s.last_ts > s.retention && t < s.last_ts - s.retention;
        if obsolete(r.start) || s.rules.iter().any(|x| obsolete(normalize(bucket_start(r.start, x.bucket, x.align)))) {
            return Err(Error::new(
                "ERR TSDB: When a series has compactions, deleting samples or compaction buckets beyond the series retention period is not possible",
            ));
        }
    }
    let deleted = series_del_range(cx, db, key, r.start, r.end);
    cx.changes(1);
    event(cx, notify::MODULE, "ts.del", db, key);
    Ok(Reply::Int(deleted as i64))
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// TS.GET key [LATEST]
fn ts_get(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    if args.len() > 3 {
        return Err(arity_error(cx.cmd.name));
    }
    let key = &args[1];
    series(cx, key, R, 0)?;
    let latest = opt(args, "LATEST").is_some();
    if args.len() == 3 && !latest {
        return Err(Error::new("ERR TSDB: wrong 3rd argument"));
    }
    let db = cx.client.db;
    let resp3 = cx.resp() == 3;
    if latest
        && src_of(cx, db, key).is_some()
        && let Some(smp) = latest_of(cx, db, key)
    {
        return Ok(sample(resp3, smp.ts, smp.v));
    }
    Ok(peek(cx, db, key).map_or(Reply::Array(Vec::new()), |s| last_point(resp3, s)))
}

/// TS.INFO key [DEBUG]
fn ts_info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    if args.len() > 3 {
        return Err(arity_error(cx.cmd.name));
    }
    let key = &args[1];
    series(cx, key, R, REFS)?;
    let debug = args[1..].iter().any(|a| a.eq_ignore_ascii_case(b"DEBUG"));
    let resp3 = cx.resp() == 3;
    let db = cx.client.db;
    let Some(s) = peek(cx, db, key) else { return Err(Error::new("ERR TSDB: the key does not exist")) };
    let (first, skipped) = s.first_valid();
    let rules: Vec<(Reply, Reply)> = s
        .rules
        .iter()
        .map(|r| (Reply::bulk(&r.dest.b), Reply::Array(vec![Reply::Int(r.bucket as i64), st(r.agg.kind.upper()), Reply::Int(r.align as i64)])))
        .collect();
    let rules = if resp3 {
        Reply::Map(rules)
    } else {
        Reply::Array(
            rules
                .into_iter()
                .map(|(d, v)| {
                    let Reply::Array(mut v) = v else { unreachable!() };
                    v.insert(0, d);
                    Reply::Array(v)
                })
                .collect(),
        )
    };
    let mut m = vec![
        (st("totalSamples"), Reply::Int(s.total.wrapping_sub(skipped) as i64)),
        (st("memoryUsage"), Reply::Int(s.mem_usage(cx.eng.ts.index.mem_usage(&s.key.b)) as i64)),
        (st("firstTimestamp"), Reply::Int(first as i64)),
        (st("lastTimestamp"), Reply::Int(s.last_ts as i64)),
        (st("retentionTime"), Reply::Int(s.retention as i64)),
        (st("chunkCount"), Reply::Int(s.chunk_count() as i64)),
        (st("chunkSize"), Reply::Int(s.chunk_size as i64)),
        (st("chunkType"), st(if s.options & OPT_UNCOMPRESSED != 0 { "uncompressed" } else { "compressed" })),
        (st("duplicatePolicy"), if s.dup != Dp::None { st(s.dup.name()) } else { Reply::Nil }),
        (st("labels"), labels_reply(resp3, &s.labels)),
        (st("sourceKey"), s.src_key.as_ref().map_or(Reply::Nil, |k| Reply::bulk(&k.b))),
        (st("rules"), rules),
        (st("ignoreMaxTimeDiff"), Reply::Int(s.ignore_time)),
        (st("ignoreMaxValDiff"), Reply::Double(s.ignore_val)),
    ];
    if debug {
        let chunks = s
            .chunks()
            .map(|(_, c)| {
                let n = c.num_samples();
                let size = c.size(false);
                let (first, last) = if n == 0 { (-1, -1) } else { (c.first_ts() as i64, c.last_ts() as i64) };
                let per = if n == 0 { 0.0 } else { (size as f32 / n as f32) as f64 };
                Reply::Map(vec![
                    (st("startTimestamp"), Reply::Int(first)),
                    (st("endTimestamp"), Reply::Int(last)),
                    (st("samples"), Reply::Int(n as i64)),
                    (st("size"), Reply::Int(size as i64)),
                    (st("bytesPerSample"), Reply::Double(per)),
                ])
            })
            .collect();
        m.push((st("keySelfName"), Reply::bulk(&s.key.b)));
        m.push((st("Chunks"), Reply::Array(chunks)));
    }
    Ok(Reply::Map(m))
}

/// TS.RANGE / TS.REVRANGE key from to [options]
fn ts_range(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    let key = &args[1];
    series(cx, key, R, ACL)?;
    let r = parse_range(cx, args, 2)?;
    let rev = cx.cmd.name == "ts.revrange";
    let db = cx.client.db;
    let resp3 = cx.resp() == 3;
    let Some(s) = peek(cx, db, key) else { return Err(Error::new("ERR TSDB: the key does not exist")) };
    let lat = latest_for(cx, s, &r);
    let reply = samples_reply(resp3, iter::query(s, &r, rev, true, lat.as_ref()), None, r.count);
    let src = src_of(cx, db, key);
    latest_used(cx, src, &lat);
    Ok(reply)
}

/// What TS.MRANGE asked for.
struct MRange {
    range: Range,
    preds: Vec<Pred>,
    with_labels: bool,
    select: Vec<Bytes>,
    /// GROUPBY's label (to its first NUL) and reducer.
    group: Option<(Vec<u8>, AggType)>,
    exclude_empty: bool,
}

/// `parseMRangeCommand`.
fn parse_mrange(cx: &Cx, args: &[Bytes]) -> Result<MRange, Error> {
    let range = parse_range(cx, args, 1)?;
    let f = arg_index(args, "FILTER").ok_or(Error::new("ERR TSDB: missing FILTER argument"))?;
    let (with_labels, select) = parse_label_query(args)?;
    let g = opt(args, "GROUPBY");
    if g.is_some_and(|g| g < f) {
        return Err(Error::new("ERR TSDB: GROUPBY should always come after filter"));
    }
    let last = g.map_or(args.len() - 1, |g| g - 1);
    let count = last.saturating_sub(f);
    if count == 0 {
        return Err(Error::new("ERR TSDB: missing labels for filter argument"));
    }
    let preds = parse_filter(&args[f + 1..f + 1 + count])?;
    let exclude_empty = opt(args, "EXCLUDEEMPTY").is_some_and(|e| g.is_none_or(|g| e != g + 1));
    if exclude_empty && g.is_some() {
        return Err(Error::new("ERR TSDB: EXCLUDEEMPTY is not allowed with GROUPBY"));
    }
    let mut group = None;
    if let Some(g) = g {
        let Some(label) = args.get(g + 1) else { return Err(arity_error(cx.cmd.name)) };
        let reduce = arg_index(args, "REDUCE");
        let Some(reduce) = reduce.filter(|_| args.len() - g == 4) else { return Err(arity_error(cx.cmd.name)) };
        let kind = args
            .get(reduce + 1)
            .and_then(|r| AggType::parse(r))
            .filter(|k| !matches!(k, AggType::First | AggType::Last | AggType::Twa))
            .ok_or(Error::new("ERR TSDB: Invalid reducer type"))?;
        if range.aggs.len() > 1 {
            return Err(Error::new("ERR TSDB: GROUPBY is not allowed when multiple aggregators are specified"));
        }
        group = Some((c_str(label).to_vec(), kind));
    }
    Ok(MRange { range, preds, with_labels, select, group, exclude_empty })
}

/// TS.MRANGE / TS.MREVRANGE from to [options] FILTER expr... [GROUPBY label REDUCE reducer]
fn ts_mrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    let mut p = parse_mrange(cx, args)?;
    let rev = cx.cmd.name == "ts.mrevrange";
    let keys = query_readable(cx, &mut p.preds)?;
    check_permissions(cx, &keys)?;
    match p.group.take() {
        Some(g) => grouped(cx, &p, keys, g, rev),
        None => ungrouped(cx, &p, keys, rev),
    }
}

/// `replyUngroupedMultiRange`.
fn ungrouped(cx: &mut Cx, p: &MRange, keys: Vec<Vec<u8>>, rev: bool) -> CmdResult {
    let resp3 = cx.resp() == 3;
    let db = cx.client.db;
    let mut out = Vec::new();
    for k in keys {
        if get_series(cx, By::Client, &k, R, SILENT).is_err() {
            continue;
        }
        let Some(s) = peek(cx, db, &k) else { continue };
        let lat = latest_for(cx, s, &p.range);
        let mut chain = iter::query(s, &p.range, rev, true, lat.as_ref());
        let mut first = None;
        if p.exclude_empty {
            loop {
                match chain.next() {
                    Some(b) if b.is_empty() => continue,
                    Some(b) => {
                        first = Some(b);
                        break;
                    }
                    None => break,
                }
            }
            if first.is_none() {
                drop(chain);
                let src = src_of(cx, db, &k);
                latest_used(cx, src, &lat);
                continue;
            }
        }
        let labels = series_labels(resp3, &s.labels, p.with_labels, &p.select);
        let samples = samples_reply(resp3, chain, first, p.range.count);
        let name = Reply::bulk(&s.key.b);
        out.push(if resp3 {
            let aggs = Reply::Map(vec![(Reply::bulk("aggregators"), Reply::Array(p.range.aggs.iter().map(|a| Reply::bulk(a.lower())).collect()))]);
            (name, Reply::Array(vec![labels, aggs, samples]))
        } else {
            (name, Reply::Array(vec![labels, samples]))
        });
        let src = src_of(cx, db, &k);
        latest_used(cx, src, &lat);
    }
    Ok(if resp3 {
        Reply::Map(out)
    } else {
        Reply::Array(
            out.into_iter()
                .map(|(n, v)| {
                    let Reply::Array(mut v) = v else { unreachable!() };
                    v.insert(0, n);
                    Reply::Array(v)
                })
                .collect(),
        )
    })
}

/// `replyGroupedMultiRange`: the series grouped by a label's value, each group reduced into
/// one series named `label=value`.
fn grouped(cx: &mut Cx, p: &MRange, keys: Vec<Vec<u8>>, (label, reducer): (Vec<u8>, AggType), rev: bool) -> CmdResult {
    let resp3 = cx.resp() == 3;
    let db = cx.client.db;
    let mut groups: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
    for k in keys {
        if get_series(cx, By::Client, &k, R, SILENT).is_err() {
            continue;
        }
        let Some(s) = peek(cx, db, &k) else { continue };
        if let Some((_, v)) = s.labels.iter().find(|(lk, _)| c_str(&lk.b) == label.as_slice()) {
            groups.entry(v.b.clone()).or_default().push(k);
        }
    }
    let mut out = Vec::new();
    for (value, members) in groups {
        // `GroupList_ApplyReducer`.
        let name = [label.as_slice(), b"=", c_str(&value)].concat();
        let c = Create { retention: 0, chunk_size: 4096, labels: None, options: OPT_UNCOMPRESSED, dup: Dp::None, ignore_time: 0, ignore_val: 0.0 };
        let mut reduced = Series::new(RStr::arg(&name), c, false);
        cx.eng.sig.ts_hook = true;
        let mut sources = Vec::new();
        let mut lats = Vec::new();
        {
            let list: Vec<&Series> = members.iter().filter_map(|k| peek(cx, db, k)).collect();
            let latest: Vec<Option<Latest>> = list.iter().map(|s| latest_for(cx, s, &p.range)).collect();
            let iters = list.iter().zip(&latest).map(|(s, l)| iter::Samples::new(s, &p.range, false, true, l.as_ref())).collect();
            for smp in iter::Reduce::new(iter::Merge::new(iters, false), reducer) {
                reduced.add_sample(smp.ts, smp.v);
            }
            for s in &list {
                sources.push(s.key.b.clone());
            }
            lats.extend(latest.into_iter().zip(list.iter().map(|s| s.src_key.as_ref().map(|k| k.b.clone()))));
        }
        for (l, src) in lats {
            latest_used(cx, src, &l);
        }
        let labels = [
            (RStr::printf(&label), RStr::printf(c_str(&value))),
            (RStr::printf(b"__reducer__"), RStr::arg(reducer.lower().as_bytes())),
            (RStr::printf(b"__source__"), RStr::raw(&sources.join(&b","[..]))),
        ];
        let zero = Range { start: 0, end: u64::MAX, aggs: Vec::new(), bucket: 0, by_ts: None, by_value: None, latest: false, ..p.range.clone() };
        let samples = samples_reply(resp3, iter::query(&reduced, &zero, rev, true, None), None, zero.count);
        let shown = if resp3 && p.with_labels { &labels[..1] } else { &labels[..] };
        let l = series_labels(resp3, shown, p.with_labels, &p.select);
        let n = Reply::bulk(&name);
        out.push(if resp3 {
            let reducers = Reply::Map(vec![(Reply::bulk("reducers"), Reply::Array(vec![Reply::bulk(reducer.lower())]))]);
            let srcs = Reply::Map(vec![(Reply::bulk("sources"), Reply::Array(sources.iter().map(Reply::bulk).collect()))]);
            (n, Reply::Array(vec![l, reducers, srcs, samples]))
        } else {
            (n, Reply::Array(vec![l, samples]))
        });
    }
    Ok(if resp3 {
        Reply::Map(out)
    } else {
        Reply::Array(
            out.into_iter()
                .map(|(n, v)| {
                    let Reply::Array(mut v) = v else { unreachable!() };
                    v.insert(0, n);
                    Reply::Array(v)
                })
                .collect(),
        )
    })
}

/// TS.NRANGE / TS.NREVRANGE numkeys key... from to [options]: the keys' samples pivoted by
/// timestamp.
fn ts_nrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    let n = parse_i64(&args[1]).filter(|&n| n > 0).ok_or(Error::new("ERR TSDB: numkeys must be a positive integer"))?;
    if n > args.len() as i64 - 4 {
        return Err(arity_error(cx.cmd.name));
    }
    let n = n as usize;
    let rs = 2 + n;
    let mut per_key = vec![1usize; n];
    let mut range;
    let agg_rel = arg_index(&args[rs..], "AGGREGATION");
    if let (true, Some(rel)) = (n > 1, agg_rel) {
        let first = rs + rel + 1;
        let count_err = Error::new("ERR TSDB: the number of AGGREGATION arguments must be equal to numkeys");
        let mut all = Vec::new();
        for (k, slot) in per_key.iter_mut().enumerate() {
            let Some(spec) = args.get(first + k).filter(|a| parse_i64(a).is_none()) else { return Err(count_err) };
            if spec.is_empty() {
                return Err(Error::new("ERR TSDB: Unknown aggregation type"));
            }
            let types = parse_agg_spec(spec)?;
            *slot = types.len();
            all.extend(types);
        }
        if args.get(first + n).is_some_and(|a| AggType::parse(a).is_some()) {
            return Err(count_err);
        }
        let tmp: Vec<Bytes> = args[..first + 1].iter().chain(&args[first + n..]).cloned().collect();
        range = parse_range(cx, &tmp, rs)?;
        range.aggs = all;
    } else {
        range = parse_range(cx, args, rs)?;
        if n == 1 {
            per_key[0] = range.aggs.len().max(1);
        }
    }
    for k in &args[2..rs] {
        series(cx, k, R, ACL)?;
    }
    let resp3 = cx.resp() == 3;
    let db = cx.client.db;
    let list: Vec<&Series> = args[2..rs].iter().filter_map(|k| peek(cx, db, k)).collect();
    let mut offset = 0;
    let mut ranges = Vec::with_capacity(n);
    for &a in &per_key {
        let mut r = range.clone();
        if range.aggs.is_empty() {
            r.aggs.clear();
        } else {
            r.aggs = range.aggs[offset..offset + a].to_vec();
            offset += a;
        }
        ranges.push(r);
    }
    let lats: Vec<Option<Latest>> = list.iter().zip(&ranges).map(|(s, r)| latest_for(cx, s, r)).collect();
    let mut iters: Vec<Box<dyn Chain + '_>> = list.iter().zip(&ranges).zip(&lats).map(|((s, r), l)| iter::query(s, r, rev_of(cx), true, l.as_ref())).collect();
    let reply = pivot(resp3, &mut iters, &per_key, range.count, rev_of(cx));
    drop(iters);
    let srcs: Vec<Option<Vec<u8>>> = list.iter().map(|s| s.src_key.as_ref().map(|k| k.b.clone())).collect();
    for (src, l) in srcs.into_iter().zip(&lats) {
        latest_used(cx, src, l);
    }
    Ok(reply)
}

fn rev_of(cx: &Cx) -> bool {
    cx.cmd.name == "ts.nrevrange"
}

fn next_nonempty(it: &mut Box<dyn Chain + '_>) -> Option<Batch> {
    loop {
        match it.next() {
            Some(b) if b.is_empty() => continue,
            other => return other,
        }
    }
}

/// `ReplySeriesNRange`: a k-way merge, one row per timestamp, NaN where a key has none.
fn pivot(resp3: bool, iters: &mut [Box<dyn Chain + '_>], per_key: &[usize], count: i64, rev: bool) -> Reply {
    let limit = if count < 0 { usize::MAX } else { count as usize };
    let mut cur: Vec<Option<(Batch, usize)>> = iters.iter_mut().map(|it| next_nonempty(it).map(|b| (b, 0))).collect();
    let mut out = Vec::new();
    while out.len() < limit {
        let target = cur
            .iter()
            .flatten()
            .map(|(b, i)| b.ts[*i])
            .reduce(|a, t| if (rev && t > a) || (!rev && t < a) { t } else { a });
        let Some(target) = target else { break };
        let mut row = Vec::new();
        for (k, slot) in cur.iter_mut().enumerate() {
            match slot {
                Some((b, i)) if b.ts[*i] == target => {
                    let vps = b.vps.max(1);
                    for j in 0..per_key[k] {
                        row.push(b.v.get(*i * vps + j).copied().unwrap_or(f64::NAN));
                    }
                    *i += 1;
                    if *i >= b.len() {
                        *slot = next_nonempty(&mut iters[k]).map(|b| (b, 0));
                    }
                }
                _ => row.extend(std::iter::repeat_n(f64::NAN, per_key[k])),
            }
        }
        out.push(Reply::Array(vec![Reply::Int(target as i64), Reply::Array(row.into_iter().map(|v| value(resp3, v)).collect())]));
    }
    Reply::Array(out)
}

/// TS.MGET [LATEST] [WITHLABELS | SELECTED_LABELS label...] FILTER expr...
fn ts_mget(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    let latest = opt(args, "LATEST").is_some();
    let Some(f) = arg_index(args, "FILTER") else { return Err(arity_error(cx.cmd.name)) };
    let (with, select) = parse_label_query(args)?;
    let mut preds = parse_filter(&args[f + 1..])?;
    let keys = query_readable(cx, &mut preds)?;
    for k in &keys {
        if let Err(e) = get_series(cx, By::Client, k, R, SILENT | ACL)
            && e.perm
        {
            return Err(perm_error());
        }
    }
    let resp3 = cx.resp() == 3;
    let db = cx.client.db;
    let mut out = Vec::new();
    for k in keys {
        if get_series(cx, By::Client, &k, R, SILENT).is_err() {
            continue;
        }
        let point = match (latest && src_of(cx, db, &k).is_some()).then(|| latest_of(cx, db, &k)).flatten() {
            Some(smp) => sample(resp3, smp.ts, smp.v),
            None => peek(cx, db, &k).map_or(Reply::Array(Vec::new()), |s| last_point(resp3, s)),
        };
        let Some(s) = peek(cx, db, &k) else { continue };
        let labels = series_labels(resp3, &s.labels, with, &select);
        out.push((Reply::bulk(&k), labels, point));
    }
    Ok(if resp3 {
        Reply::Map(out.into_iter().map(|(k, l, p)| (k, Reply::Array(vec![l, p]))).collect())
    } else {
        Reply::Array(out.into_iter().map(|(k, l, p)| Reply::Array(vec![k, l, p])).collect())
    })
}

/// TS.QUERYINDEX expr...
fn ts_queryindex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    let mut preds = parse_filter(&args[1..])?;
    let (keys, _) = cx.eng.ts.index.query(&mut preds, None);
    Ok(Reply::Set(keys.iter().map(Reply::bulk).collect()))
}

/// TS.QUERYLABELS LABELS | VALUES label [FILTER expr...]
fn ts_querylabels(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    let values = if c_eq(&args[1], "LABELS") {
        false
    } else if c_eq(&args[1], "VALUES") {
        true
    } else {
        return Err(Error::new("ERR TSDB: unknown subtype, must be one of LABELS|VALUES"));
    };
    let (label, fs) = if values {
        let Some(l) = args.get(2) else { return Err(arity_error(cx.cmd.name)) };
        (Some(l.clone()), 3)
    } else {
        (None, 2)
    };
    let mut preds = None;
    if args.len() > fs {
        if !args[fs].eq_ignore_ascii_case(b"FILTER") {
            return Err(Error::new("ERR TSDB: unknown argument, expected FILTER"));
        }
        if args.len() - 1 - fs == 0 {
            return Err(Error::new("ERR TSDB: FILTER given with no filter expressions"));
        }
        preds = Some(parse_filter(&args[fs + 1..])?);
    }
    let ix = &cx.eng.ts.index;
    let candidates = match preds.as_mut() {
        Some(p) => ix.query(p, None).0,
        None => ix.all(),
    };
    let user = cx.client.user.clone();
    let mut agg = std::collections::BTreeSet::new();
    for c in candidates {
        if !user.can_use_key(&c, false) {
            continue;
        }
        agg.extend(ix.labels_of(&c, label.as_deref()));
    }
    Ok(Reply::Set(agg.iter().map(Reply::bulk).collect()))
}

/// TS.READ key timestamp [BLOCK ms min_count] [MAX_COUNT n]: the samples from a cursor on,
/// waiting for `min_count` of them with BLOCK.
fn ts_read(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    drain(cx);
    let o = &args[3..];
    let block = arg_index(o, "BLOCK").is_some();
    let max_given = arg_index(o, "MAX_COUNT").is_some();
    if args.len() != 3 + if block { 3 } else { 0 } + if max_given { 2 } else { 0 } {
        return Err(arity_error(cx.cmd.name));
    }
    let (mut timeout, mut min, mut max) = (0i64, 1i64, -1i64);
    if block {
        let v = after(o, "BLOCK", 2).map(|a| (parse_i64(&a[0]), parse_i64(&a[1])));
        let Some((Some(t), Some(m))) = v.filter(|(t, _)| t.is_some_and(|t| t >= 0)) else {
            return Err(Error::new("TSDB: BLOCK milliseconds must be a non-negative integer"));
        };
        if m <= 0 {
            return Err(Error::new("TSDB: BLOCK min_count must be a positive integer"));
        }
        (timeout, min) = (t, m);
    }
    if max_given {
        max = after(o, "MAX_COUNT", 1).and_then(|a| parse_i64(&a[0])).filter(|&m| m > 0).ok_or(Error::new("TSDB: MAX_COUNT must be a positive integer"))?;
    }
    if max != -1 && min > max {
        return Err(Error::new("TSDB: BLOCK min_count must be <= MAX_COUNT"));
    }
    let key = &args[1];
    let db = cx.client.db;
    let ts = args[2].as_ref();
    let cursor = if ts == b"-" {
        0
    } else if ts == b"+" || ts == b"$" {
        match open_key(cx, db, key, false) {
            Found::Empty => 0,
            Found::Other => return Err(WRONGTYPE),
            Found::Ts => match peek(cx, db, key) {
                Some(s) if s.total == 0 => 0,
                Some(s) if ts == b"+" => s.last_ts,
                Some(s) => s.last_ts.saturating_add(1),
                None => 0,
            },
        }
    } else {
        timestamp(ts).ok_or(Error::new("TSDB: invalid timestamp"))?
    };
    let rerun = cx.mode == Mode::Unblock;
    let strict = if rerun && cx.timing_out() {
        false
    } else if rerun {
        key_exists(cx, key)
    } else {
        block
    };
    if let Some(r) = read_try(cx, key, cursor, min as usize, max, strict) {
        return r;
    }
    if !cx.can_block() {
        return Err(Error::new("TSDB: blocking TS.READ (with BLOCK) is not allowed inside MULTI, EVAL, or a deny-blocking context"));
    }
    let mut again = vec![args[0].clone(), key.clone(), Bytes::from(cursor.to_string()), Bytes::from_static(b"BLOCK"), Bytes::from(timeout.to_string()), Bytes::from(min.to_string())];
    if max_given {
        again.extend([Bytes::from_static(b"MAX_COUNT"), Bytes::from(max.to_string())]);
    }
    Ok(cx.block_module(key, timeout, again))
}

/// `TSDB_read_try_reply`: the answer now, or `None` (strict) while fewer than `min` samples
/// qualify.
fn read_try(cx: &mut Cx, key: &[u8], cursor: u64, min: usize, max: i64, strict: bool) -> Option<CmdResult> {
    let db = cx.client.db;
    let empty = || (!strict).then(|| Ok(Reply::Array(Vec::new())));
    match open_key(cx, db, key, false) {
        Found::Empty => empty(),
        Found::Other => Some(Err(WRONGTYPE)),
        Found::Ts => {
            let resp3 = cx.resp() == 3;
            let s = peek(cx, db, key)?;
            if s.last_ts < cursor {
                return empty();
            }
            let r = Range { start: cursor, end: i64::MAX as u64, count: max, ..Range::default() };
            if strict {
                let mut chain = iter::query(s, &r, false, true, None);
                let mut total = 0;
                while total < min {
                    match chain.next() {
                        Some(b) => total += b.len(),
                        None => break,
                    }
                }
                if total < min {
                    return None;
                }
            }
            Some(Ok(samples_reply(resp3, iter::query(s, &r, false, true, None), None, max)))
        }
    }
}
