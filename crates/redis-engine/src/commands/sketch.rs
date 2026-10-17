//! Count-min sketch, top-k and t-digest commands (CMS.*, TOPK.*, TDIGEST.*), as the RedisBloom
//! module bundled with Redis 8.10 answers them (key handling as in [`super::bloom`]).
//!
//! T-digest reads compress the digest's buffered samples in place (`td_compress`), so they
//! change what TDIGEST.INFO reports without being writes.

use super::bloom::{Open, arg_index, double, info_map, nested_error, open_read, open_write, quiet, reopen, store};
use super::*;
use crate::bloom::cms::Cms;
use crate::bloom::tdigest::TDigest;
use crate::bloom::topk::TopK;
use crate::bloom::{Sketch, SketchKind};
use crate::cmd;
use crate::util::parse_i64;

pub static COMMANDS: &[Command] = &[
    cmd!("cms.initbydim", 4, WRITE | DENYOOM | MODULE | ACL_FAST, 1, 1, 1, cms_init, Module),
    cmd!("cms.initbyprob", 4, WRITE | DENYOOM | MODULE | ACL_FAST, 1, 1, 1, cms_init, Module),
    cmd!("cms.incrby", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, cms_incrby, Module),
    cmd!("cms.query", -3, READONLY | MODULE, 1, 1, 1, cms_query, Module),
    cmd!("cms.merge", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, cms_merge, Module),
    cmd!("cms.info", 2, READONLY | MODULE | ACL_FAST, 1, 1, 1, cms_info, Module),
    cmd!("topk.reserve", -3, WRITE | DENYOOM | MODULE | ACL_FAST, 1, 1, 1, topk_reserve, Module),
    cmd!("topk.add", -3, WRITE | DENYOOM | MODULE, 1, 1, 1, topk_add, Module),
    cmd!("topk.incrby", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, topk_incrby, Module),
    cmd!("topk.query", -3, READONLY | MODULE, 1, 1, 1, topk_query, Module),
    cmd!("topk.count", -3, READONLY | MODULE, 1, 1, 1, topk_count, Module),
    cmd!("topk.list", -2, READONLY | MODULE, 1, 1, 1, topk_list, Module),
    cmd!("topk.info", 2, READONLY | MODULE | ACL_FAST, 1, 1, 1, topk_info, Module),
    cmd!("tdigest.create", -2, WRITE | DENYOOM | MODULE | ACL_FAST, 1, 1, 1, td_create, Module),
    cmd!("tdigest.add", -3, WRITE | DENYOOM | MODULE, 1, 1, 1, td_add, Module),
    cmd!("tdigest.reset", 2, WRITE | DENYOOM | MODULE | ACL_FAST, 1, 1, 1, td_reset, Module),
    cmd!("tdigest.merge", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, keys = td_merge_keys, td_merge, Module),
    cmd!("tdigest.min", 2, READONLY | MODULE | ACL_FAST, 1, 1, 1, td_min, Module),
    cmd!("tdigest.max", 2, READONLY | MODULE | ACL_FAST, 1, 1, 1, td_max, Module),
    cmd!("tdigest.quantile", -3, READONLY | MODULE | ACL_FAST, 1, 1, 1, td_quantile, Module),
    cmd!("tdigest.byrank", -3, READONLY | MODULE | ACL_FAST, 1, 1, 1, td_byrank, Module),
    cmd!("tdigest.byrevrank", -3, READONLY | MODULE | ACL_FAST, 1, 1, 1, td_byrank, Module),
    cmd!("tdigest.rank", -3, READONLY | MODULE | ACL_FAST, 1, 1, 1, td_rank, Module),
    cmd!("tdigest.revrank", -3, READONLY | MODULE | ACL_FAST, 1, 1, 1, td_rank, Module),
    cmd!("tdigest.cdf", -3, READONLY | MODULE | ACL_FAST, 1, 1, 1, td_cdf, Module),
    cmd!("tdigest.trimmed_mean", 4, READONLY | MODULE, 1, 1, 1, td_trimmed_mean, Module),
    cmd!("tdigest.info", 2, READONLY | MODULE | ACL_FAST, 1, 1, 1, td_info, Module),
];

/// A key's sketch of `kind` for a command that needs one (`GetCMSKey` and the like).
fn need<T>(o: Open<T>, missing: &'static str) -> Result<T, Error> {
    match o {
        Open::Found(x) => Ok(x),
        Open::Empty => Err(Error::new(missing)),
        Open::Other => Err(WRONGTYPE),
    }
}

/// `RedisModule_ReplyWithCString`: up to the first NUL.
fn c_str(b: &[u8]) -> Bytes {
    Bytes::copy_from_slice(b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n]))
}

// ---------------------------------------------------------------------------------------------
// CMS.*
// ---------------------------------------------------------------------------------------------

const CMS_MISSING: &str = "CMS: key does not exist";

/// CMS.INITBYDIM key width depth, CMS.INITBYPROB key error probability
fn cms_init(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let key = &args[1];
    if !matches!(open_write(cx, key, SketchKind::Cms), Open::Empty) {
        return Err(Error::new("CMS: key already exists"));
    }
    let (width, depth) = if cx.cmd.name == "cms.initbydim" {
        let width = parse_i64(&args[2]).filter(|w| *w >= 1).ok_or(Error::new("CMS: invalid width"))?;
        let depth = parse_i64(&args[3]).filter(|d| *d >= 1).ok_or(Error::new("CMS: invalid depth"))?;
        (width, depth)
    } else {
        let error = double(&args[2]).filter(|e| *e > 0.0 && *e < 1.0).ok_or(Error::new("CMS: invalid overestimation value"))?;
        let prob = double(&args[3]).filter(|p| *p > 0.0 && *p < 1.0).ok_or(Error::new("CMS: invalid prob value"))?;
        let (w, d) = Cms::dims(error, prob);
        (w as i64, d as i64)
    };
    if width < 1 || depth < 1 || width > i64::MAX / depth {
        return Err(Error::new("CMS: invalid init arguments"));
    }
    let cms = Cms::new(width as u64, depth as u64).ok_or(Error::new("CMS: Insufficient memory to create the key"))?;
    store(cx, key, Sketch::Cms(cms));
    cx.changes(1);
    Ok(Reply::Ok)
}

/// CMS.INCRBY key item increment [item increment ...]
fn cms_incrby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    if args.len() % 2 == 1 {
        return Err(arity_error(cx.cmd.name));
    }
    let key = &args[1];
    need(open_write(cx, key, SketchKind::Cms), CMS_MISSING)?;
    let mut pairs = Vec::with_capacity(args.len() / 2 - 1);
    for p in args[2..].chunks(2) {
        let v = parse_i64(&p[1]).ok_or(Error::new("CMS: Cannot parse number"))?;
        if v < 0 {
            return Err(Error::new("CMS: Number cannot be negative"));
        }
        pairs.push((&p[0], v as u64));
    }
    let mut out = Vec::with_capacity(pairs.len());
    for (item, v) in pairs {
        let count = reopen(cx, key).sketch().cms_mut().incr_by(item, v);
        out.push(if count != u32::MAX as u64 { Reply::Int(count as i64) } else { nested_error(cx, "CMS: INCRBY overflow") });
    }
    cx.changes(1);
    Ok(Reply::Array(out))
}

/// CMS.QUERY key item [item ...]
fn cms_query(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let cms = need(open_read(cx, &args[1], SketchKind::Cms), CMS_MISSING)?.cms();
    Ok(Reply::Array(args[2..].iter().map(|i| Reply::Int(cms.query(i) as i64)).collect()))
}

/// CMS.MERGE destination numkeys source [source ...] [WEIGHTS weight [weight ...]]
fn cms_merge(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let dest = &args[1];
    let (width, depth) = {
        need(open_write(cx, dest, SketchKind::Cms), CMS_MISSING)?;
        let mut k = reopen(cx, dest);
        let c = k.sketch().cms();
        (c.width, c.depth)
    };
    let numkeys = parse_i64(&args[2]).ok_or(Error::new("CMS: invalid numkeys"))?;
    let rest = &args[3..];
    if numkeys <= 0 {
        return Err(Error::new("CMS: Number of keys must be positive"));
    }
    let weights_at = arg_index(rest, "WEIGHTS");
    match weights_at {
        None if numkeys != rest.len() as i64 => return Err(Error::new("CMS: wrong number of keys")),
        Some(p) if p as i64 != numkeys || rest.len() as i64 != 1 + numkeys * 2 => {
            return Err(Error::new("CMS: wrong number of keys/weights"));
        }
        _ => {}
    }
    let n = numkeys as usize;
    // Sources other than the destination are copied: the destination changes under them.
    let mut srcs: Vec<(Option<Cms>, i64)> = Vec::with_capacity(n);
    for (i, key) in rest[..n].iter().enumerate() {
        let w = match weights_at {
            None => 1,
            Some(_) => parse_i64(&rest[n + 1 + i]).ok_or(Error::new("CMS: invalid weight value"))?,
        };
        let c = need(open_read(cx, key, SketchKind::Cms), CMS_MISSING)?.cms();
        if c.width != width || c.depth != depth {
            return Err(Error::new("CMS: width/depth is not equal"));
        }
        srcs.push(((key != dest).then(|| c.clone()), w));
    }
    let srcs: Vec<(Option<&Cms>, i64)> = srcs.iter().map(|(c, w)| (c.as_ref(), *w)).collect();
    if !reopen(cx, dest).sketch().cms_mut().merge(&srcs) {
        return Err(Error::new("CMS: MERGE overflow"));
    }
    cx.changes(1);
    Ok(Reply::Ok)
}

/// CMS.INFO key
fn cms_info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let c = need(open_read(cx, &args[1], SketchKind::Cms), CMS_MISSING)?.cms();
    Ok(info_map(vec![("width", Reply::Int(c.width as i64)), ("depth", Reply::Int(c.depth as i64)), ("count", Reply::Int(c.counter as i64))]))
}

// ---------------------------------------------------------------------------------------------
// TOPK.*
// ---------------------------------------------------------------------------------------------

const TOPK_MISSING: &str = "TopK: key does not exist";

/// TOPK.RESERVE key topk [width depth decay]
fn topk_reserve(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    if args.len() != 3 && args.len() != 6 {
        return Err(arity_error(cx.cmd.name));
    }
    let key = &args[1];
    if !matches!(open_write(cx, key, SketchKind::TopK), Open::Empty) {
        return Err(Error::new("TopK: key already exists"));
    }
    let dim = |a: &[u8], what: &'static str| parse_i64(a).filter(|v| (1..=u32::MAX as i64).contains(v)).map(|v| v as u32).ok_or(Error::new(what));
    let k = dim(&args[2], "TopK: invalid k")?;
    let (width, depth, decay) = if args.len() == 6 {
        let width = dim(&args[3], "TopK: invalid width")?;
        let depth = dim(&args[4], "TopK: invalid depth")?;
        let decay = double(&args[5]).filter(|d| *d > 0.0 && *d <= 1.0).ok_or(Error::new("TopK: invalid decay value. must be '<= 1' & '> 0'"))?;
        (width, depth, decay)
    } else {
        (8, 7, 0.9)
    };
    let topk = TopK::new(k, width, depth, decay).ok_or(Error::new("ERR Insufficient memory to create topk data structure"))?;
    store(cx, key, Sketch::TopK(topk));
    cx.changes(1);
    Ok(Reply::Ok)
}

fn expelled(item: Option<Box<[u8]>>) -> Reply {
    item.map_or(Reply::Nil, |i| Reply::Bulk(c_str(&i)))
}

/// TOPK.ADD key item [item ...]
fn topk_add(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    need(open_write(cx, &args[1], SketchKind::TopK), TOPK_MISSING)?;
    let mut k = reopen(cx, &args[1]);
    let t = k.sketch().topk_mut();
    let out = args[2..].iter().map(|i| expelled(t.add(i, 1))).collect();
    k.changes(1);
    Ok(Reply::Array(out))
}

/// TOPK.INCRBY key item increment [item increment ...]
fn topk_incrby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    if args.len() % 2 == 1 {
        return Err(arity_error(cx.cmd.name));
    }
    let key = &args[1];
    need(open_write(cx, key, SketchKind::TopK), TOPK_MISSING)?;
    let mut out = Vec::with_capacity(args.len() / 2 - 1);
    for p in args[2..].chunks(2) {
        let Some(incr) = parse_i64(&p[1]).filter(|v| (0..=100_000).contains(v)) else {
            // The module stops here, short of the array it announced.
            out.push(nested_error(
                cx,
                "TopK: increment must be an integer greater or equal to 0                            and smaller or equal to 100,000",
            ));
            break;
        };
        out.push(expelled(reopen(cx, key).sketch().topk_mut().add(&p[0], incr as u32)));
    }
    cx.changes(1);
    Ok(Reply::Array(out))
}

/// TOPK.QUERY key item [item ...]
fn topk_query(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let resp3 = cx.resp() == 3;
    let t = need(open_read(cx, &args[1], SketchKind::TopK), TOPK_MISSING)?.topk();
    Ok(Reply::Array(
        args[2..]
            .iter()
            .map(|i| {
                let found = t.query(i);
                if resp3 { Reply::Bool(found) } else { Reply::Int(found as i64) }
            })
            .collect(),
    ))
}

/// TOPK.COUNT key item [item ...]
fn topk_count(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let t = need(open_read(cx, &args[1], SketchKind::TopK), TOPK_MISSING)?.topk();
    Ok(Reply::Array(args[2..].iter().map(|i| Reply::Int(t.count(i) as i64)).collect()))
}

/// `strncasecmp(arg, "WITHCOUNT", len(arg)) == 0`: a prefix of the keyword (C strings).
fn withcount(arg: &[u8]) -> bool {
    const WORD: &[u8] = b"WITHCOUNT";
    for (i, &c) in arg.iter().enumerate() {
        let w = WORD.get(i).copied().unwrap_or(0);
        if !c.eq_ignore_ascii_case(&w) {
            return false;
        }
        if c == 0 {
            return true;
        }
    }
    true
}

/// TOPK.LIST key [WITHCOUNT]
fn topk_list(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 3 {
        return Err(arity_error(cx.cmd.name));
    }
    let counts = match args.get(2) {
        Some(a) if withcount(a) => true,
        Some(_) => return Err(Error::new("WITHCOUNT keyword expected")),
        None => false,
    };
    let t = need(open_read(cx, &args[1], SketchKind::TopK), TOPK_MISSING)?.topk();
    let mut out = Vec::new();
    for b in t.list().into_iter().take(t.k as usize) {
        if b.count == 0 {
            break;
        }
        out.push(Reply::Bulk(c_str(b.item.as_deref().unwrap_or_default())));
        if counts {
            out.push(Reply::Int(b.count as i64));
        }
    }
    Ok(Reply::Array(out))
}

/// TOPK.INFO key
fn topk_info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let t = need(open_read(cx, &args[1], SketchKind::TopK), TOPK_MISSING)?.topk();
    Ok(info_map(vec![
        ("k", Reply::Int(t.k as i64)),
        ("width", Reply::Int(t.width as i64)),
        ("depth", Reply::Int(t.depth as i64)),
        ("decay", Reply::Double(t.decay)),
    ]))
}

// ---------------------------------------------------------------------------------------------
// TDIGEST.*
// ---------------------------------------------------------------------------------------------

const TD_MISSING: &str = "ERR T-Digest: key does not exist";
const TD_OVERFLOW: Error = Error::new("ERR T-Digest: overflow detected");
const TD_DEFAULT_COMPRESSION: i64 = 100;

/// `_TDigest_ParseCompressionParameter`.
fn compression(a: &[u8]) -> Result<i64, Error> {
    let c = parse_i64(a).ok_or(Error::new("ERR T-Digest: error parsing compression parameter"))?;
    if c <= 0 {
        return Err(Error::new("ERR T-Digest: compression parameter needs to be a positive integer"));
    }
    Ok(c)
}

/// TDIGEST.CREATE key [COMPRESSION compression]
fn td_create(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    if args.len() != 2 && args.len() != 4 {
        return Err(arity_error(cx.cmd.name));
    }
    let key = &args[1];
    match open_write(cx, key, SketchKind::TDigest) {
        Open::Empty => {}
        Open::Found(_) => return Err(Error::new("ERR T-Digest: key already exists")),
        Open::Other => return Err(WRONGTYPE),
    }
    let mut c = TD_DEFAULT_COMPRESSION;
    if args.len() == 4 {
        if arg_index(&args[2..], "COMPRESSION").is_none() {
            return Err(Error::new("ERR T-Digest: wrong keyword"));
        }
        c = compression(&args[3])?;
    }
    let t = TDigest::new(c as f64).ok_or(Error::new("ERR T-Digest: allocation failed"))?;
    store(cx, key, Sketch::TDigest(t));
    cx.changes(1);
    Ok(Reply::Ok)
}

/// TDIGEST.RESET key
fn td_reset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    need(open_write(cx, &args[1], SketchKind::TDigest), TD_MISSING)?;
    let mut k = reopen(cx, &args[1]);
    k.sketch().tdigest_mut().reset();
    k.changes(1);
    Ok(Reply::Ok)
}

/// TDIGEST.ADD key value [value ...]
fn td_add(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    need(open_write(cx, &args[1], SketchKind::TDigest), TD_MISSING)?;
    let mut k = reopen(cx, &args[1]);
    let mut vals = Vec::with_capacity(args.len() - 2);
    for a in &args[2..] {
        let v = double(a).ok_or(Error::new("ERR T-Digest: error parsing val parameter"))?;
        if !v.is_finite() {
            return Err(Error::new("ERR T-Digest: val parameter needs to be a finite number"));
        }
        vals.push(v);
    }
    let t = k.sketch().tdigest_mut();
    for v in vals {
        t.add(v, 1).map_err(|_| TD_OVERFLOW)?;
    }
    k.changes(1);
    Ok(Reply::Ok)
}

/// TDIGEST.MERGE's keys: the destination and the `numkeys` sources.
fn td_merge_keys(args: &[Bytes]) -> Vec<usize> {
    let mut keys = vec![1];
    if let Some(n) = args.get(2).and_then(|a| parse_i64(a)).filter(|n| *n > 0) {
        keys.extend((3..args.len()).take(n as usize));
    }
    keys
}

/// TDIGEST.MERGE destination numkeys source [source ...] [COMPRESSION c] [OVERRIDE]
fn td_merge(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let dest = &args[1];
    let start = match open_read(cx, dest, SketchKind::TDigest) {
        Open::Empty => None,
        Open::Other => return Err(WRONGTYPE),
        Open::Found(x) => Some(x.tdigest().compression),
    };
    let numkeys = parse_i64(&args[2]).ok_or(Error::new("ERR T-Digest: error parsing numkeys"))?;
    if numkeys <= 0 {
        return Err(Error::new("ERR T-Digest: numkeys needs to be a positive integer"));
    }
    if numkeys > args.len() as i64 - 3 {
        return Err(arity_error(cx.cmd.name));
    }
    let n = numkeys as usize;
    let mut c = start.map_or(0, |c| c as i64);
    let mut use_max = start.is_none();
    let mut over = false;
    let rest = &args[3 + n..];
    if !rest.is_empty() {
        let at = arg_index(rest, "COMPRESSION");
        if let Some(i) = at {
            use_max = false;
            let v = rest.get(i + 1).ok_or_else(|| arity_error(cx.cmd.name))?;
            c = compression(v)?;
        }
        if arg_index(rest, "OVERRIDE").is_some() {
            over = true;
            if at.is_none() {
                use_max = true;
                c = 0;
            }
        } else if at.is_none() {
            return Err(Error::new("ERR T-Digest: wrong keyword"));
        }
    }
    let srcs = &args[3..3 + n];
    for key in srcs {
        let sc = if key == dest {
            start.ok_or(Error::new(TD_MISSING))?
        } else {
            need(open_read(cx, key, SketchKind::TDigest), TD_MISSING)?.tdigest().compression
        };
        if use_max {
            c = c.max(sc as i64);
        }
    }
    let mut to = TDigest::new(c as f64).ok_or(Error::new("ERR T-Digest: allocation of destination digest failed"))?;
    // Merging compresses each source in place.
    let mut merge = |cx: &mut Cx, key: &[u8]| {
        let mut k = quiet(cx, key);
        to.merge_from(k.sketch().tdigest_mut())
    };
    if start.is_some() && !over {
        let _ = merge(cx, dest);
    }
    for key in srcs {
        merge(cx, key).map_err(|_| TD_OVERFLOW)?;
    }
    // A write lookup, then the value replaced.
    cx.touch(dest);
    store(cx, dest, Sketch::TDigest(to));
    cx.changes(1);
    Ok(Reply::Ok)
}

/// A digest for a read command: counted lookup, then quiet access (it compresses).
fn td_read<'c>(cx: &'c mut Cx, key: &[u8]) -> Result<crate::cx::KeyMut<'c>, Error> {
    need(open_read(cx, key, SketchKind::TDigest), TD_MISSING)?;
    Ok(quiet(cx, key))
}

/// TDIGEST.MIN key, TDIGEST.MAX key
fn td_min(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let t = need(open_read(cx, &args[1], SketchKind::TDigest), TD_MISSING)?.tdigest();
    Ok(Reply::Double(if t.size() > 0 { t.min } else { f64::NAN }))
}

fn td_max(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let t = need(open_read(cx, &args[1], SketchKind::TDigest), TD_MISSING)?.tdigest();
    Ok(Reply::Double(if t.size() > 0 { t.max } else { f64::NAN }))
}

/// `_halfRoundDown`.
fn half_round_down(f: f64) -> f64 {
    let int = f.trunc();
    if (f - int).abs() <= 0.5 {
        int
    } else if int >= 0.0 {
        int + 1.0
    } else {
        int - 1.0
    }
}

/// TDIGEST.RANK / TDIGEST.REVRANK key value [value ...]
fn td_rank(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let reverse = cx.cmd.name == "tdigest.revrank";
    let mut k = td_read(cx, &args[1])?;
    let mut vals = Vec::with_capacity(args.len() - 2);
    for a in &args[2..] {
        vals.push(double(a).ok_or(Error::new("ERR T-Digest: error parsing value"))?);
    }
    let t = k.sketch().tdigest_mut();
    let size = t.size() as f64;
    let (min, max) = (t.min, t.max);
    let ranks = vals
        .into_iter()
        .map(|v| {
            let r = if size == 0.0 {
                -2.0
            } else if v < min {
                if reverse { size } else { -1.0 }
            } else if v > max {
                if reverse { -1.0 } else { size }
            } else {
                let at = t.cdf(v) * size;
                if reverse { (size - at.round()).round() } else { half_round_down(at) }
            };
            Reply::Int(r as i64)
        })
        .collect();
    Ok(Reply::Array(ranks))
}

/// TDIGEST.BYRANK / TDIGEST.BYREVRANK key rank [rank ...]
fn td_byrank(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let reverse = cx.cmd.name == "tdigest.byrevrank";
    let mut k = td_read(cx, &args[1])?;
    let mut ranks = Vec::with_capacity(args.len() - 2);
    for a in &args[2..] {
        let r = parse_i64(a).ok_or(Error::new("ERR T-Digest: error parsing rank"))?;
        if r < 0 {
            return Err(Error::new("ERR T-Digest: rank needs to be non negative"));
        }
        ranks.push(r as f64);
    }
    let t = k.sketch().tdigest_mut();
    let size = t.size() as f64;
    let (min, max) = (t.min, t.max);
    let values = ranks
        .into_iter()
        .map(|r| {
            Reply::Double(if size == 0.0 {
                f64::NAN
            } else if r == 0.0 {
                if reverse { max } else { min }
            } else if r >= size {
                if reverse { f64::NEG_INFINITY } else { f64::INFINITY }
            } else {
                t.quantile((if reverse { size - r - 1.0 } else { r }) / size)
            })
        })
        .collect();
    Ok(Reply::Array(values))
}

/// TDIGEST.QUANTILE key quantile [quantile ...]
fn td_quantile(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut k = td_read(cx, &args[1])?;
    let mut qs = Vec::with_capacity(args.len() - 2);
    for a in &args[2..] {
        let q = double(a).ok_or(Error::new("ERR T-Digest: error parsing quantile"))?;
        if q < 0.0 || q > 1.0 {
            return Err(Error::new("ERR T-Digest: quantile should be in [0,1]"));
        }
        qs.push(q);
    }
    let t = k.sketch().tdigest_mut();
    let mut out = Vec::with_capacity(qs.len());
    // Ascending runs answered together (`td_quantiles`).
    let mut i = 0;
    while i < qs.len() {
        let start = i;
        while i + 1 < qs.len() && qs[i] <= qs[i + 1] {
            i += 1;
        }
        out.extend(t.quantiles(&qs[start..=i]).into_iter().map(Reply::Double));
        i += 1;
    }
    Ok(Reply::Array(out))
}

/// TDIGEST.CDF key value [value ...]
fn td_cdf(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut k = td_read(cx, &args[1])?;
    let mut vals = Vec::with_capacity(args.len() - 2);
    for a in &args[2..] {
        vals.push(double(a).ok_or(Error::new("ERR T-Digest: error parsing cdf"))?);
    }
    let t = k.sketch().tdigest_mut();
    Ok(Reply::Array(vals.into_iter().map(|v| Reply::Double(t.cdf(v))).collect()))
}

/// TDIGEST.TRIMMED_MEAN key low_cut_percentile high_cut_percentile
fn td_trimmed_mean(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut k = td_read(cx, &args[1])?;
    let low = double(&args[2]).ok_or(Error::new("ERR T-Digest: error parsing low_cut_percentile"))?;
    let high = double(&args[3]).ok_or(Error::new("ERR T-Digest: error parsing high_cut_percentile"))?;
    if !(0.0..=1.0).contains(&low) || !(0.0..=1.0).contains(&high) {
        return Err(Error::new("ERR T-Digest: low_cut_percentile and high_cut_percentile should be in [0,1]"));
    }
    if low >= high {
        return Err(Error::new("ERR T-Digest: low_cut_percentile should be lower than high_cut_percentile"));
    }
    Ok(Reply::Double(k.sketch().tdigest_mut().trimmed_mean(low, high)))
}

/// TDIGEST.INFO key
fn td_info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let t = need(open_read(cx, &args[1], SketchKind::TDigest), TD_MISSING)?.tdigest();
    Ok(info_map(vec![
        ("Compression", Reply::Int(t.compression as i64)),
        ("Capacity", Reply::Int(t.cap as i64)),
        ("Merged nodes", Reply::Int(t.merged_nodes as i64)),
        ("Unmerged nodes", Reply::Int(t.unmerged_nodes as i64)),
        ("Merged weight", Reply::Int(t.merged_weight)),
        ("Unmerged weight", Reply::Int(t.unmerged_weight)),
        ("Observations", Reply::Int(t.unmerged_weight.wrapping_add(t.merged_weight))),
        ("Total compressions", Reply::Int(t.total_compressions)),
        ("Memory usage", Reply::Int(t.mem_usage() as i64)),
    ]))
}
