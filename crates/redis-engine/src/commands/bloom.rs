//! Bloom and cuckoo filter commands (BF.*, CF.*), as the RedisBloom module bundled with Redis
//! 8.10 answers them, and what the module's other commands (CMS.*, TOPK.*, TDIGEST.*) share.
//!
//! The module's conventions are kept: a key opened for reading is a counted lookup (hits,
//! misses, `keymiss`), a key opened for writing is signalled as modified whatever the command
//! then does, a new value is stored like `RM_ModuleTypeSetValue` (any old value and its TTL
//! dropped, a `new` event and no other), and a write propagates only when the module replicates
//! it (`RM_ReplicateVerbatim`: one dirty change), even if it then answers an error.

use super::zsets::zfloat;
use super::*;
use crate::bloom::cuckoo::{self, Cuckoo, Insert};
use crate::bloom::sb::{self, AddError, Chain, InitError};
use crate::bloom::{Sketch, SketchKind};
use crate::cmd;
use crate::config::{BloomOpt, bloom_error_rate, bloom_int};
use crate::cx::KeyMut;
use crate::util::{fmt_g, parse_i64};
use crate::value::Value;

pub static COMMANDS: &[Command] = &[
    cmd!("bf.reserve", -4, WRITE | DENYOOM | MODULE | ACL_FAST, 1, 1, 1, bf_reserve, Module),
    cmd!("bf.add", 3, WRITE | DENYOOM | MODULE, 1, 1, 1, bf_add, Module),
    cmd!("bf.madd", -3, WRITE | DENYOOM | MODULE, 1, 1, 1, bf_add, Module),
    cmd!("bf.insert", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, bf_insert, Module),
    cmd!("bf.exists", 3, READONLY | MODULE | FAST, 1, 1, 1, bf_exists, Module),
    cmd!("bf.mexists", -3, READONLY | MODULE | FAST, 1, 1, 1, bf_exists, Module),
    cmd!("bf.info", -2, READONLY | MODULE | ACL_FAST | FAST, 1, 1, 1, bf_info, Module),
    cmd!("bf.card", 2, READONLY | MODULE | ACL_FAST | FAST, 1, 1, 1, bf_card, Module),
    cmd!("bf.debug", 2, READONLY | MODULE | FAST, 1, 1, 1, bf_debug, Module),
    cmd!("bf.scandump", 3, READONLY | MODULE | FAST, 1, 1, 1, bf_scandump, Module),
    cmd!("bf.loadchunk", 4, WRITE | DENYOOM | MODULE, 1, 1, 1, bf_loadchunk, Module),
    cmd!("cf.reserve", -3, WRITE | DENYOOM | MODULE | ACL_FAST, 1, 1, 1, cf_reserve, Module),
    cmd!("cf.add", 3, WRITE | DENYOOM | MODULE, 1, 1, 1, cf_add, Module),
    cmd!("cf.addnx", 3, WRITE | DENYOOM | MODULE, 1, 1, 1, cf_add, Module),
    cmd!("cf.insert", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, cf_insert, Module),
    cmd!("cf.insertnx", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, cf_insert, Module),
    cmd!("cf.exists", 3, READONLY | MODULE | FAST, 1, 1, 1, cf_check, Module),
    cmd!("cf.mexists", -3, READONLY | MODULE | FAST, 1, 1, 1, cf_check, Module),
    cmd!("cf.count", 3, READONLY | MODULE | FAST, 1, 1, 1, cf_check, Module),
    cmd!("cf.del", 3, WRITE | MODULE | FAST, 1, 1, 1, cf_del, Module),
    cmd!("cf.compact", -1, READONLY | MODULE | FAST, 1, 1, 1, cf_compact, Module),
    cmd!("cf.scandump", 3, READONLY | MODULE | FAST, 1, 1, 1, cf_scandump, Module),
    cmd!("cf.loadchunk", 4, WRITE | DENYOOM | MODULE, 1, 1, 1, cf_loadchunk, Module),
    cmd!("cf.info", 2, READONLY | MODULE | ACL_FAST | FAST, 1, 1, 1, cf_info, Module),
    cmd!("cf.debug", 2, READONLY | MODULE | FAST, 1, 1, 1, cf_debug, Module),
];

// ---------------------------------------------------------------------------------------------
// Shared by the module's commands
// ---------------------------------------------------------------------------------------------

/// What a module finds under a key (`getValue`'s statuses).
pub(super) enum Open<T> {
    Empty,
    /// Another type.
    Other,
    Found(T),
}

/// `RM_OpenKey(READ)`: a counted lookup.
pub(super) fn open_read<'c>(cx: &'c mut Cx, key: &[u8], kind: SketchKind) -> Open<&'c Sketch> {
    match cx.get(key) {
        None => Open::Empty,
        Some(Value::Sketch(x)) if x.kind() == kind => Open::Found(x),
        Some(_) => Open::Other,
    }
}

/// `RM_OpenKey(WRITE)`: an uncounted lookup; the key is signalled as modified when closed.
/// The value itself is then reached with [`reopen`].
pub(super) fn open_write(cx: &mut Cx, key: &[u8], kind: SketchKind) -> Open<()> {
    cx.touch(key);
    match cx.peek(key) {
        None => Open::Empty,
        Some(e) if matches!(&e.value, Value::Sketch(x) if x.kind() == kind) => Open::Found(()),
        Some(_) => Open::Other,
    }
}

/// A key [`open_write`] found (or a value just stored), again.
pub(super) fn reopen<'c>(cx: &'c mut Cx, key: &[u8]) -> KeyMut<'c> {
    let mut k = cx.get_mut(key).expect("opened");
    k.unchanged();
    k
}

/// A sketch a read command changes internally (a t-digest compressing): no access time, no
/// signal, no propagation.
pub(super) fn quiet<'c>(cx: &'c mut Cx, key: &[u8]) -> KeyMut<'c> {
    cx.get_mut_quiet(key).expect("opened")
}

/// `RM_ModuleTypeSetValue`: whatever the key held goes (with its TTL); a `new` event.
pub(super) fn store(cx: &mut Cx, key: &[u8], x: Sketch) {
    cx.delete(key);
    cx.set(key, Value::Sketch(Box::new(x)), false);
}

/// `RedisModule_ReplyWithMapOrArray`'s replies: a map in RESP3, the values alone otherwise when
/// only values are listed.
pub(super) fn named(cx: &Cx, name: &str, v: Reply) -> Reply {
    if cx.resp() == 3 { Reply::Map(vec![(Reply::status(name), v)]) } else { Reply::Array(vec![v]) }
}

/// A map of names to values (RESP2: the flat array).
pub(super) fn info_map(fields: Vec<(&str, Reply)>) -> Reply {
    Reply::Map(fields.into_iter().map(|(n, v)| (Reply::status(n), v)).collect())
}

/// An error answered inside an array: counted like any error reply.
pub(super) fn nested_error(cx: &mut Cx, msg: &str) -> Reply {
    let r = Reply::error(msg);
    cx.eng.count_error(&r);
    r
}

/// `RMUtil_ArgIndex`: the first argument (the command name and key included) equal to `word`.
pub(super) fn arg_index(args: &[Bytes], word: &str) -> Option<usize> {
    args.iter().position(|a| a.eq_ignore_ascii_case(word.as_bytes()))
}

/// `RM_StringToDouble` (`string2d`).
pub(super) fn double(b: &[u8]) -> Option<f64> {
    zfloat(b)
}

fn status_error(found: bool, other: bool) -> Error {
    if other {
        WRONGTYPE
    } else if found {
        Error::new("ERR item exists")
    } else {
        Error::new("ERR not found")
    }
}

/// `statusStrerror` for a key that should hold the type.
fn missing<T>(o: &Open<T>) -> Error {
    status_error(false, matches!(o, Open::Other))
}

const MAX_SCANDUMP_SIZE: u64 = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// BF.*
// ---------------------------------------------------------------------------------------------

const CREATE_OOM: Error = Error::new("ERR Insufficient memory to create filter");

struct BfOptions {
    capacity: i64,
    error: f64,
    autocreate: bool,
    expansion: i64,
    non_scaling: bool,
}

impl BfOptions {
    fn defaults(cx: &Cx) -> Self {
        let expansion = bloom_int(&cx.eng.cfg, BloomOpt::BfExpansion).value;
        BfOptions {
            capacity: bloom_int(&cx.eng.cfg, BloomOpt::BfInitialSize).value,
            error: bloom_error_rate(&cx.eng.cfg),
            autocreate: true,
            expansion,
            non_scaling: expansion == 0,
        }
    }

    /// `bfCreateChain`.
    fn chain(&self) -> Result<Chain, Error> {
        let scaling = if self.non_scaling { sb::NO_SCALING } else { 0 };
        Chain::new(self.capacity as u64, self.error, sb::FORCE64 | scaling | sb::NOROUND, self.expansion as u32).map_err(|e| match e {
            InitError::Oom => CREATE_OOM,
            InitError::Invalid => Error::new("ERR could not create filter"),
        })
    }
}

/// BF.RESERVE key error_rate capacity [EXPANSION expansion] [NONSCALING]
fn bf_reserve(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    if args.len() > 7 {
        return Err(arity_error(cx.cmd.name));
    }
    let mut o = BfOptions::defaults(cx);
    o.error = double(&args[2]).ok_or(Error::new("ERR bad error rate"))?;
    if !(o.error > 0.0 && o.error < 1.0) {
        return Err(Error::msg(format!("ERR error rate must be in the range ({:.6}, {:.6})", 0.0, 1.0)));
    }
    o.error = o.error.min(0.25);
    o.capacity = parse_i64(&args[3]).ok_or(Error::new("ERR bad capacity"))?;
    let size = bloom_int(&cx.eng.cfg, BloomOpt::BfInitialSize);
    if !size.valid(o.capacity) {
        return Err(Error::msg(format!("ERR capacity must be in the range [{}, {}]", size.min, size.max)));
    }
    if arg_index(args, "NONSCALING").is_some() {
        o.non_scaling = true;
    }
    let ex = arg_index(args, "EXPANSION");
    if ex == Some(args.len() - 1) {
        return Err(Error::new("ERR no expansion"));
    }
    if let Some(i) = ex {
        o.expansion = parse_i64(&args[i + 1]).ok_or(Error::new("ERR bad expansion"))?;
        if o.expansion == 0 {
            o.non_scaling = true;
        } else if o.non_scaling {
            return Err(Error::new("Nonscaling filters cannot expand"));
        }
        let r = bloom_int(&cx.eng.cfg, BloomOpt::BfExpansion);
        if !r.valid(o.expansion) {
            return Err(Error::msg(format!("ERR expansion must be in the range [{}, {}]", r.min, r.max)));
        }
    }
    let key = &args[1];
    match open_write(cx, key, SketchKind::Bloom) {
        Open::Empty => {}
        Open::Other => return Err(WRONGTYPE),
        Open::Found(_) => return Err(status_error(true, false)),
    }
    let chain = o.chain()?;
    store(cx, key, Sketch::Bloom(chain));
    cx.changes(1);
    Ok(Reply::Ok)
}

/// `bfInsertCommon`.
fn bf_insert_common(cx: &mut Cx, key: &[u8], items: &[Bytes], o: &BfOptions, multi: bool) -> CmdResult {
    match open_write(cx, key, SketchKind::Bloom) {
        Open::Empty if o.autocreate => {
            let chain = o.chain()?;
            store(cx, key, Sketch::Bloom(chain));
        }
        Open::Found(_) => {}
        found => return Err(missing(&found)),
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let r = reopen(cx, key).sketch().bloom_mut().add(item);
        match r {
            Ok(new) => out.push(Reply::Bool(new)),
            Err(AddError::Full) => {
                out.push(nested_error(cx, "ERR non scaling filter is full"));
                break;
            }
            Err(AddError::Failed) => out.push(nested_error(cx, "ERR problem inserting into filter")),
        }
    }
    cx.changes(1);
    if multi {
        return Ok(Reply::Array(out));
    }
    match out.pop() {
        Some(Reply::Error(e)) => {
            // Counted already: answered as it is.
            Ok(Reply::Error(e))
        }
        Some(r) => Ok(r),
        None => Ok(Reply::Array(vec![])),
    }
}

/// BF.ADD key item, BF.MADD key item [item ...]
fn bf_add(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let o = BfOptions::defaults(cx);
    let multi = cx.cmd.name == "bf.madd";
    bf_insert_common(cx, &args[1], &args[2..], &o, multi)
}

/// BF.INSERT key [CAPACITY c] [ERROR e] [EXPANSION x] [NOCREATE] [NONSCALING] ITEMS item [item ...]
fn bf_insert(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut o = BfOptions::defaults(cx);
    let arity = || arity_error("bf.insert");
    let mut pos = 2;
    let mut items = None;
    while pos < args.len() && items.is_none() {
        let a = &args[pos];
        let at = |i: usize| a.get(i).map_or(0, u8::to_ascii_lowercase);
        match at(0) {
            b'i' => {
                pos += 1;
                items = Some(pos);
            }
            b'e' => {
                pos += 1;
                if pos == args.len() {
                    return Err(arity());
                }
                if at(1) == b'r' {
                    o.error = double(&args[pos]).filter(|e| *e > 0.0 && *e < 1.0).ok_or(Error::new("Bad error rate"))?.min(0.25);
                } else {
                    let r = bloom_int(&cx.eng.cfg, BloomOpt::BfExpansion);
                    o.expansion = parse_i64(&args[pos]).filter(|v| r.valid(*v)).ok_or(Error::new("Bad expansion"))?;
                }
                pos += 1;
            }
            b'c' => {
                pos += 1;
                if pos == args.len() {
                    return Err(arity());
                }
                let r = bloom_int(&cx.eng.cfg, BloomOpt::BfInitialSize);
                o.capacity = parse_i64(&args[pos]).filter(|v| r.valid(*v)).ok_or(Error::new("Bad capacity"))?;
                pos += 1;
            }
            b'n' => {
                if at(2) == b'c' {
                    o.autocreate = false;
                } else {
                    o.non_scaling = true;
                }
                pos += 1;
            }
            _ => return Err(Error::new("Unknown argument received")),
        }
    }
    if o.expansion == 0 {
        o.non_scaling = true;
    }
    let items = match items {
        Some(i) if i < args.len() => i,
        _ => return Err(arity()),
    };
    bf_insert_common(cx, &args[1], &args[items..], &o, true)
}

/// BF.EXISTS key item, BF.MEXISTS key item [item ...]: false for anything not a filter.
fn bf_exists(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let multi = cx.cmd.name == "bf.mexists";
    let found = match open_read(cx, &args[1], SketchKind::Bloom) {
        Open::Found(x) => Some(x.bloom()),
        _ => None,
    };
    let out: Vec<Reply> = args[2..].iter().map(|i| Reply::Bool(found.is_some_and(|c| c.check(i)))).collect();
    Ok(if multi { Reply::Array(out) } else { out.into_iter().next().unwrap_or(Reply::Bool(false)) })
}

/// BF.INFO key [CAPACITY | SIZE | FILTERS | ITEMS | EXPANSION]
fn bf_info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 3 {
        return Err(arity_error(cx.cmd.name));
    }
    let fields = match open_read(cx, &args[1], SketchKind::Bloom) {
        Open::Found(x) => {
            let c = x.bloom();
            let expansion = if c.options & sb::NO_SCALING != 0 { Reply::Nil } else { Reply::Int(c.growth as i64) };
            [
                ("capacity", "Capacity", Reply::Int(c.capacity() as i64)),
                ("size", "Size", Reply::Int(c.mem_usage() as i64)),
                ("filters", "Number of filters", Reply::Int(c.links.len() as i64)),
                ("items", "Number of items inserted", Reply::Int(c.size as i64)),
                ("expansion", "Expansion rate", expansion),
            ]
        }
        o => return Err(missing(&o)),
    };
    if let Some(want) = args.get(2) {
        return match fields.into_iter().find(|(k, _, _)| want.eq_ignore_ascii_case(k.as_bytes())) {
            Some((_, name, v)) => Ok(named(cx, name, v)),
            None => Err(Error::new("Invalid information value")),
        };
    }
    Ok(info_map(fields.into_iter().map(|(_, n, v)| (n, v)).collect()))
}

/// BF.CARD key: 0 for a missing key.
fn bf_card(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    match open_read(cx, &args[1], SketchKind::Bloom) {
        Open::Empty => Ok(Reply::Int(0)),
        Open::Found(x) => Ok(Reply::Int(x.bloom().size as i64)),
        o => Err(missing(&o)),
    }
}

/// BF.DEBUG key
fn bf_debug(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let c = match open_read(cx, &args[1], SketchKind::Bloom) {
        Open::Found(x) => x.bloom(),
        o => return Err(missing(&o)),
    };
    let width = if c.options & sb::FORCE64 != 0 { 64 } else { 32 };
    let mut out = vec![Reply::bulk(format!("size:{}", c.size))];
    for l in &c.links {
        let bits = if l.bits != 0 { l.bits } else { 1u64 << l.n2 };
        out.push(Reply::bulk(format!(
            "bytes:{} bits:{} hashes:{} hashwidth:{} capacity:{} size:{} ratio:{}",
            l.bytes(),
            bits,
            l.hashes,
            width,
            l.entries,
            l.size,
            fmt_g(l.error)
        )));
    }
    Ok(Reply::Array(out))
}

/// BF.SCANDUMP key iterator
fn bf_scandump(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let c = match open_read(cx, &args[1], SketchKind::Bloom) {
        Open::Found(x) => x.bloom(),
        o => return Err(missing(&o)),
    };
    let iter = parse_i64(&args[2]).ok_or(Error::new("Second argument must be numeric"))?;
    if iter == 0 {
        return Ok(Reply::Array(vec![Reply::Int(1), Reply::Bulk(c.header().into())]));
    }
    Ok(match c.chunk(iter, MAX_SCANDUMP_SIZE) {
        Some((next, data)) => Reply::Array(vec![Reply::Int(next), Reply::Bulk(Bytes::copy_from_slice(data))]),
        None => Reply::Array(vec![Reply::Int(0), Reply::Bulk(Bytes::new())]),
    })
}

/// BF.LOADCHUNK key iterator data
fn bf_loadchunk(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let iter = parse_i64(&args[2]).ok_or(Error::new("ERR Second argument must be numeric"))?;
    let (key, buf) = (&args[1], &args[3]);
    match open_write(cx, key, SketchKind::Bloom) {
        Open::Empty if iter == 1 => {
            let chain = Chain::from_header(buf).ok_or(Error::new(sb::BAD_DATA))?;
            store(cx, key, Sketch::Bloom(chain));
            cx.changes(1);
            Ok(Reply::Ok)
        }
        Open::Found(()) => {
            let mut k = reopen(cx, key);
            k.sketch().bloom_mut().load_chunk(iter, buf).map_err(Error::new)?;
            k.changes(1);
            Ok(Reply::Ok)
        }
        o => Err(missing(&o)),
    }
}

// ---------------------------------------------------------------------------------------------
// CF.*
// ---------------------------------------------------------------------------------------------

/// `cfCreate`.
fn cf_create(capacity: i64, bucket_size: i64, max_iterations: i64, expansion: i64) -> Result<Cuckoo, Error> {
    if capacity < bucket_size * 2 {
        return Err(Error::new("ERR Could not create filter"));
    }
    Cuckoo::new(capacity as u64, bucket_size as u16, max_iterations as u16, expansion as u16).ok_or(CREATE_OOM)
}

/// A CF.RESERVE option's value (`RMUtil_ArgIndex` anywhere, the value after it).
fn cf_option(args: &[Bytes], word: &str, r: crate::config::BloomInt) -> Result<i64, Error> {
    let Some(i) = arg_index(args, word) else { return Ok(r.value) };
    let v = args.get(i + 1).and_then(|a| parse_i64(a)).ok_or_else(|| Error::msg(format!("Couldn't parse {word}")))?;
    if !r.valid(v) {
        return Err(Error::msg(format!("{word}: value must be in the range [{}, {}]", r.min, r.max)));
    }
    Ok(v)
}

/// CF.RESERVE key capacity [BUCKETSIZE b] [MAXITERATIONS m] [EXPANSION e]
fn cf_reserve(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    if args.len().is_multiple_of(2) {
        return Err(arity_error(cx.cmd.name));
    }
    let capacity = parse_i64(&args[2]).ok_or(Error::new("Bad capacity"))?;
    let cfg = &cx.eng.cfg;
    let max_iterations = cf_option(args, "MAXITERATIONS", bloom_int(cfg, BloomOpt::CfMaxIterations))?;
    let bucket_size = cf_option(args, "BUCKETSIZE", bloom_int(cfg, BloomOpt::CfBucketSize))?;
    let expansion = cf_option(args, "EXPANSION", bloom_int(cfg, BloomOpt::CfExpansion))?;
    let max = bloom_int(cfg, BloomOpt::CfInitialSize).max;
    if bucket_size * 2 > capacity || capacity > max {
        return Err(Error::msg(format!("Capacity must be in the range [2 * BUCKETSIZE, {max}]")));
    }
    let key = &args[1];
    match open_write(cx, key, SketchKind::Cuckoo) {
        Open::Empty => {}
        Open::Other => return Err(WRONGTYPE),
        Open::Found(_) => return Err(status_error(true, false)),
    }
    let cf = cf_create(capacity, bucket_size, max_iterations, expansion)?;
    store(cx, key, Sketch::Cuckoo(cf));
    cx.changes(1);
    Ok(Reply::Ok)
}

struct CfOptions {
    nx: bool,
    autocreate: bool,
    multi: bool,
    capacity: i64,
}

/// `cfInsertCommon`.
fn cf_insert_common(cx: &mut Cx, key: &[u8], items: &[Bytes], o: &CfOptions) -> CmdResult {
    let resp3 = cx.resp() == 3;
    let cfg = &cx.eng.cfg;
    let (bs, mi, ex) =
        (bloom_int(cfg, BloomOpt::CfBucketSize).value, bloom_int(cfg, BloomOpt::CfMaxIterations).value, bloom_int(cfg, BloomOpt::CfExpansion).value);
    let max_expansions = bloom_int(cfg, BloomOpt::CfMaxExpansions).value;
    let mut created = false;
    match open_write(cx, key, SketchKind::Cuckoo) {
        Open::Empty if o.autocreate => {
            let cf = cf_create(o.capacity, bs, mi, ex)?;
            store(cx, key, Sketch::Cuckoo(cf));
            created = true;
        }
        Open::Found(_) => {}
        found => return Err(missing(&found)),
    }
    let replicate_created = |cx: &mut Cx| {
        if created {
            cx.changes(1);
        }
    };
    if reopen(cx, key).sketch().cuckoo().filters.len() as i64 >= max_expansions {
        replicate_created(cx);
        return Err(Error::new("Maximum expansions reached"));
    }
    // RESP3 answers booleans, except CF.INSERTNX.
    let booleans = resp3 && (!o.nx || !o.multi);
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let h = cuckoo::hash(item);
        let r = {
            let mut k = reopen(cx, key);
            let cf = k.sketch().cuckoo_mut();
            if o.nx { cf.insert_unique(h) } else { cf.insert(h) }
        };
        match r {
            Insert::Inserted | Insert::Exists => {
                let added = r == Insert::Inserted;
                out.push(if booleans { Reply::Bool(added) } else { Reply::Int(added as i64) });
            }
            Insert::NoSpace | Insert::MemAllocFailed if !o.multi => {
                replicate_created(cx);
                return Err(Error::new(if r == Insert::NoSpace { "Filter is full" } else { "ERR Insufficient memory" }));
            }
            Insert::NoSpace | Insert::MemAllocFailed => {
                out.push(if resp3 && !o.nx { Reply::Bool(false) } else { Reply::Int(-1) });
            }
        }
    }
    cx.changes(1);
    Ok(if o.multi { Reply::Array(out) } else { out.into_iter().next().expect("one item") })
}

/// CF.ADD key item, CF.ADDNX key item
fn cf_add(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let capacity = bloom_int(&cx.eng.cfg, BloomOpt::CfInitialSize).value;
    let o = CfOptions { nx: cx.cmd.name == "cf.addnx", autocreate: true, multi: false, capacity };
    cf_insert_common(cx, &args[1], &args[2..3], &o)
}

/// CF.INSERT / CF.INSERTNX key [CAPACITY c] [NOCREATE] ITEMS item [item ...]
fn cf_insert(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let size = bloom_int(&cx.eng.cfg, BloomOpt::CfInitialSize);
    let mut o = CfOptions { nx: cx.cmd.name == "cf.insertnx", autocreate: true, multi: true, capacity: size.value };
    let name = cx.cmd.name;
    let mut pos = 2;
    let mut items = None;
    while pos < args.len() && items.is_none() {
        match args[pos].first().map_or(0, u8::to_ascii_lowercase) {
            b'c' => {
                pos += 1;
                if pos == args.len() {
                    return Err(arity_error(name));
                }
                o.capacity = parse_i64(&args[pos]).ok_or(Error::new("Bad capacity"))?;
                if !size.valid(o.capacity) {
                    return Err(Error::msg(format!("Capacity must be in the range [{} * 2, {}]", BloomOpt::CfBucketSize.name(), size.max)));
                }
                pos += 1;
            }
            b'i' => {
                pos += 1;
                items = Some(pos);
            }
            b'n' => {
                o.autocreate = false;
                pos += 1;
            }
            _ => return Err(Error::new("Unknown argument received")),
        }
    }
    let items = match items {
        Some(i) if i < args.len() => i,
        _ => return Err(arity_error(name)),
    };
    cf_insert_common(cx, &args[1], &args[items..], &o)
}

/// CF.EXISTS / CF.MEXISTS / CF.COUNT key item...: nothing found for anything not a filter.
fn cf_check(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let multi = cx.cmd.name == "cf.mexists";
    let count = cx.cmd.name == "cf.count";
    let booleans = cx.resp() == 3 && !count;
    let found = match open_read(cx, &args[1], SketchKind::Cuckoo) {
        Open::Found(x) => Some(x.cuckoo()),
        _ => None,
    };
    let out: Vec<Reply> = args[2..]
        .iter()
        .map(|item| {
            let h = cuckoo::hash(item);
            let n = match found {
                None => 0,
                Some(cf) if count => cf.count(h) as i64,
                Some(cf) => cf.check(h) as i64,
            };
            if booleans { Reply::Bool(n != 0) } else { Reply::Int(n) }
        })
        .collect();
    Ok(if multi { Reply::Array(out) } else { out.into_iter().next().unwrap_or(Reply::Int(0)) })
}

/// CF.DEL key item
fn cf_del(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let resp3 = cx.resp() == 3;
    let Open::Found(()) = open_write(cx, &args[1], SketchKind::Cuckoo) else {
        return Err(Error::new("Not found"));
    };
    let mut k = reopen(cx, &args[1]);
    k.changes(1);
    let deleted = k.sketch().cuckoo_mut().delete(cuckoo::hash(&args[2]));
    Ok(if resp3 { Reply::Bool(deleted) } else { Reply::Int(deleted as i64) })
}

/// CF.COMPACT key
fn cf_compact(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    if args.len() != 2 {
        return Err(arity_error(cx.cmd.name));
    }
    let Open::Found(()) = open_write(cx, &args[1], SketchKind::Cuckoo) else {
        return Err(Error::new("Cuckoo filter was not found"));
    };
    let mut k = reopen(cx, &args[1]);
    k.sketch().cuckoo_mut().compact(true);
    k.changes(1);
    Ok(Reply::Ok)
}

/// CF.SCANDUMP key iterator
fn cf_scandump(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let pos = parse_i64(&args[2]).filter(|p| *p >= 0).ok_or(Error::new("Invalid position"))?;
    let cf = match open_read(cx, &args[1], SketchKind::Cuckoo) {
        Open::Found(x) => x.cuckoo(),
        o => return Err(missing(&o)),
    };
    let end = || Reply::Array(vec![Reply::Int(0), Reply::Nil]);
    if cf.num_items == 0 {
        return Ok(end());
    }
    if pos == 0 {
        return Ok(Reply::Array(vec![Reply::Int(1), Reply::Bulk(cf.header().into())]));
    }
    Ok(match cf.chunk(pos, MAX_SCANDUMP_SIZE) {
        Some((next, data)) => Reply::Array(vec![Reply::Int(next), Reply::Bulk(Bytes::copy_from_slice(data))]),
        None => end(),
    })
}

/// CF.LOADCHUNK key iterator data
fn cf_loadchunk(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let key = &args[1];
    let open = open_write(cx, key, SketchKind::Cuckoo);
    let pos = parse_i64(&args[2]).filter(|p| *p != 0).ok_or(Error::new("Invalid position"))?;
    let blob = &args[3];
    if pos == 1 {
        match open {
            Open::Empty => {}
            Open::Other => return Err(WRONGTYPE),
            Open::Found(_) => return Err(status_error(true, false)),
        }
        if blob.len() != cuckoo::HEADER {
            return Err(Error::new("Invalid header"));
        }
        let cf = Cuckoo::from_header(blob).ok_or(Error::new("Couldn't create filter!"))?;
        store(cx, key, Sketch::Cuckoo(cf));
        cx.changes(1);
        return Ok(Reply::Ok);
    }
    let Open::Found(()) = open else { return Err(missing(&open)) };
    let mut k = reopen(cx, key);
    if !k.sketch().cuckoo_mut().load_chunk(pos, blob) {
        return Err(Error::new("Couldn't load chunk!"));
    }
    k.changes(1);
    Ok(Reply::Ok)
}

/// CF.INFO key
fn cf_info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let cf = match open_read(cx, &args[1], SketchKind::Cuckoo) {
        Open::Found(x) => x.cuckoo(),
        o => return Err(missing(&o)),
    };
    Ok(info_map(vec![
        ("Size", Reply::Int(cf.mem_usage() as i64)),
        ("Number of buckets", Reply::Int(cf.num_buckets as i64)),
        ("Number of filters", Reply::Int(cf.filters.len() as i64)),
        ("Number of items inserted", Reply::Int(cf.num_items as i64)),
        ("Number of items deleted", Reply::Int(cf.num_deletes as i64)),
        ("Bucket size", Reply::Int(cf.bucket_size as i64)),
        ("Expansion rate", Reply::Int(cf.expansion as i64)),
        ("Max iterations", Reply::Int(cf.max_iterations as i64)),
    ]))
}

/// CF.DEBUG key
fn cf_debug(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let cf = match open_read(cx, &args[1], SketchKind::Cuckoo) {
        Open::Found(x) => x.cuckoo(),
        o => return Err(missing(&o)),
    };
    Ok(Reply::bulk(format!(
        "bktsize:{} buckets:{} items:{} deletes:{} filters:{} max_iterations:{} expansion:{}",
        cf.bucket_size,
        cf.num_buckets,
        cf.num_items,
        cf.num_deletes,
        cf.filters.len(),
        cf.max_iterations,
        cf.expansion
    )))
}
