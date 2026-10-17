//! Vector set commands (VADD, VSIM, VREM…), as the `vectorset` module built into Redis 8.10
//! answers them. The module registers them in uppercase, with no ACL categories, and keeps the
//! module key conventions of [`super::bloom`]: reads are counted lookups, a key opened for
//! writing is signalled as modified, a write propagates only when the module replicates it.
//! Searches the module runs on a thread (VSIM, VADD CAS) run inline here: same answers.

use super::bloom::Open;
use super::zsets::zfloat;
use super::*;
use crate::cmd;
use crate::util::parse_i64;
use crate::value::Value;
use crate::vset::expr::Expr;
use crate::vset::hnsw::{self, Node};
use crate::vset::{self as vs, VSet};

pub static COMMANDS: &[Command] = &[
    cmd!("VADD", -5, WRITE | DENYOOM | MODULE, 1, 1, 1, vadd, Module),
    cmd!("VREM", 3, WRITE | MODULE, 1, 1, 1, vrem, Module),
    cmd!("VSIM", -4, READONLY | MODULE, 1, 1, 1, vsim, Module),
    cmd!("VDIM", 2, READONLY | MODULE | FAST, 1, 1, 1, vdim, Module),
    cmd!("VCARD", 2, READONLY | MODULE | FAST, 1, 1, 1, vcard, Module),
    cmd!("VEMB", -3, READONLY | MODULE | FAST, 1, 1, 1, vemb, Module),
    cmd!("VLINKS", -3, READONLY | MODULE | FAST, 1, 1, 1, vlinks, Module),
    cmd!("VINFO", 2, READONLY | MODULE | FAST, 1, 1, 1, vinfo, Module),
    cmd!("VSETATTR", 4, WRITE | MODULE | FAST, 1, 1, 1, vsetattr, Module),
    cmd!("VGETATTR", 3, READONLY | MODULE | FAST, 1, 1, 1, vgetattr, Module),
    cmd!("VRANDMEMBER", -2, READONLY | MODULE, 1, 1, 1, vrandmember, Module),
    cmd!("VISMEMBER", 3, READONLY | MODULE, 1, 1, 1, vismember, Module),
    cmd!("VRANGE", -4, READONLY | MODULE, 1, 1, 1, vrange, Module),
];

const INVALID_VECTOR: Error = Error::new("ERR invalid vector specification");
/// VSIM's search width when it is given none.
const DEFAULT_SEARCH_EF: u64 = 100;
const DEFAULT_COUNT: i64 = 10;

fn wrong_arity(cx: &Cx) -> Error {
    arity_error(cx.cmd.name)
}

/// `RM_OpenKey(READ)`: a counted lookup.
fn open_read<'c>(cx: &'c mut Cx, key: &[u8]) -> Open<&'c VSet> {
    match cx.get(key) {
        None => Open::Empty,
        Some(Value::VSet(v)) => Open::Found(v),
        Some(_) => Open::Other,
    }
}

/// `RM_OpenKey(WRITE)`: uncounted, and the key is signalled as modified.
fn open_write(cx: &mut Cx, key: &[u8]) -> Open<()> {
    cx.touch(key);
    match cx.peek(key) {
        None => Open::Empty,
        Some(e) if matches!(e.value, Value::VSet(_)) => Open::Found(()),
        Some(_) => Open::Other,
    }
}

/// The set a successful open found, for reading.
fn set_ref<'c>(cx: &'c mut Cx, key: &[u8]) -> &'c VSet {
    match cx.peek(key).map(|e| &e.value) {
        Some(Value::VSet(v)) => v,
        _ => unreachable!("opened"),
    }
}

/// The set a successful open found, to change: the open already signalled it.
fn set_mut<'c>(cx: &'c mut Cx, key: &[u8]) -> crate::cx::KeyMut<'c> {
    let mut k = cx.get_mut(key).expect("opened");
    k.unchanged();
    k
}

/// The set a read command searches (searching marks visited nodes): no access time, no signal.
fn set_search<'c>(cx: &'c mut Cx, key: &[u8]) -> crate::cx::KeyMut<'c> {
    cx.get_mut_quiet(key).expect("opened")
}

fn is(a: &[u8], word: &str) -> bool {
    a.eq_ignore_ascii_case(word.as_bytes())
}

/// A parsed vector argument (`parseVector`).
struct Parsed {
    vec: Vec<f32>,
    dim: u32,
    /// REDUCE's dimension (0 without).
    reduce: u32,
    /// Arguments taken, REDUCE included.
    consumed: usize,
}

/// `parseVector`: `[REDUCE dim] FP32 blob | VALUES n v1 … vn` from `start`.
fn parse_vector(args: &[Bytes], start: usize, allow_reduce: bool) -> Option<Parsed> {
    let argc = args.len();
    let mut start = start;
    let mut consumed = 0;
    let mut reduce = 0u32;
    if allow_reduce && argc > start + 2 && is(&args[start], "REDUCE") {
        let r = parse_i64(&args[start + 1]).filter(|&r| r > 0)?;
        reduce = r as u32;
        start += 2;
        consumed += 2;
    }
    let format = args.get(start)?;
    let vec: Vec<f32> = if is(format, "FP32") {
        if argc < start + 2 {
            return None;
        }
        let blob = &args[start + 1];
        if !blob.len().is_multiple_of(4) || blob.len() < 4 || (blob.len() / 4) as u64 > vs::MAX_DIM {
            return None;
        }
        consumed += 2;
        blob.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
    } else if is(format, "VALUES") {
        if argc < start + 2 {
            return None;
        }
        let n = parse_i64(&args[start + 1]).filter(|&n| n >= 1 && n as u64 <= vs::MAX_DIM)? as usize;
        if argc < start + 2 + n {
            return None;
        }
        let v = args[start + 2..start + 2 + n].iter().map(|a| zfloat(a).map(|d| d as f32)).collect::<Option<Vec<f32>>>()?;
        consumed += n + 2;
        v
    } else {
        return None;
    };
    let dim = vec.len() as u32;
    if reduce != 0 && reduce > dim {
        return None;
    }
    Some(Parsed { vec, dim, reduce, consumed })
}

/// VADD key [REDUCE dim] FP32|VALUES vector element [CAS] [NOQUANT|BIN|Q8] [EF n] [SETATTR json] [M n]
fn vadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let p = parse_vector(args, 2, true).ok_or(INVALID_VECTOR)?;
    let argc = args.len();
    if argc < 3 + p.consumed {
        return Err(wrong_arity(cx));
    }
    let mut ef = vs::DEFAULT_CREATE_EF as i64;
    let mut m = hnsw::DEFAULT_M as i64;
    let mut quant = hnsw::QUANT_Q8;
    let mut attrib: Option<&[u8]> = None;
    let mut j = 2 + p.consumed + 1;
    while j < argc {
        let opt = &args[j];
        if is(opt, "CAS") {
        } else if is(opt, "EF") && j + 1 < argc {
            ef = parse_i64(&args[j + 1]).filter(|&e| e > 0 && e <= 1_000_000).ok_or(Error::new("ERR invalid EF"))?;
            j += 1;
        } else if is(opt, "M") && j + 1 < argc {
            m = parse_i64(&args[j + 1])
                .filter(|&v| v >= hnsw::MIN_M as i64 && v <= hnsw::MAX_M as i64)
                .ok_or(Error::new("ERR invalid M"))?;
            j += 1;
        } else if is(opt, "SETATTR") && j + 1 < argc {
            attrib = Some(&args[j + 1]);
            j += 1;
        } else if is(opt, "NOQUANT") {
            quant = hnsw::QUANT_NONE;
        } else if is(opt, "BIN") {
            quant = hnsw::QUANT_BIN;
        } else if is(opt, "Q8") {
            quant = hnsw::QUANT_Q8;
        } else {
            return Err(Error::new("ERR invalid option after element"));
        }
        j += 1;
    }
    let key = &args[1];
    let item = &args[2 + p.consumed];
    let Parsed { mut vec, dim, reduce, .. } = p;
    match open_write(cx, key) {
        Open::Other => return Err(WRONGTYPE),
        Open::Empty => {
            let mut set = VSet::new(if reduce != 0 { reduce } else { dim }, quant, m as u32);
            if reduce != 0 {
                let matrix = vs::fp::projection_matrix(dim, reduce);
                vec = vs::fp::project(&vec, &matrix, reduce);
                set.proj = Some(matrix.into());
                set.proj_input = dim;
            }
            cx.delete(key);
            cx.set(key, Value::VSet(Box::new(set)), false);
        }
        Open::Found(()) => {
            let set = set_ref(cx, key);
            let g = &set.hnsw.g;
            if g.quant != quant {
                return Err(Error::new("ERR asked quantization mismatch with existing vector set"));
            }
            if g.m as i64 != m {
                return Err(Error::new("ERR asked M value mismatch with existing vector set"));
            }
            if (set.proj.is_none() && g.dim != dim) || (set.proj.is_some() && g.dim != reduce) {
                return Err(Error::msg(format!("ERR Vector dimension mismatch - got {} but set has {}", dim as i32, g.dim as i32)));
            }
            if reduce != 0 {
                if set.proj.is_none() {
                    return Err(Error::new("ERR cannot add projection to existing set without projection"));
                }
                if reduce != g.dim {
                    return Err(Error::new("ERR projection dimension mismatch with existing set"));
                }
            }
            if set.proj.is_some() {
                if dim != set.proj_input {
                    return Err(Error::msg(format!(
                        "ERR Input dimension mismatch for projection - got {} but projection expects {}",
                        dim as i32, set.proj_input as i32
                    )));
                }
                vec = set.project(&vec);
            }
        }
    }
    let mut k = set_mut(cx, key);
    let added = k.vset().insert(&vec, item, attrib, ef as u32);
    drop(k);
    if added {
        cx.changes(1);
    }
    Ok(Reply::Bool(added))
}

/// VREM key element
fn vrem(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let key = &args[1];
    match open_write(cx, key) {
        Open::Empty => return Ok(Reply::Bool(false)),
        Open::Other => return Err(WRONGTYPE),
        Open::Found(()) => {}
    }
    let mut k = set_mut(cx, key);
    if !k.vset().remove(&args[2]) {
        return Ok(Reply::Bool(false));
    }
    let empty = k.vset().is_empty();
    drop(k);
    if empty {
        // RM_DeleteKey: no event.
        cx.delete(key);
    }
    cx.changes(1);
    Ok(Reply::Bool(true))
}

/// One VSIM answer: element, score, attributes.
type Hit = (Bytes, f64, Option<Bytes>);

/// VSIM key ELE|FP32|VALUES vector-or-element [WITHSCORES] [WITHATTRIBS] [COUNT n] [EPSILON d]
/// [EF n] [FILTER expr] [FILTER-EF n] [TRUTH] [NOTHREAD]
fn vsim(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let resp3 = cx.resp() == 3;
    match open_read(cx, key) {
        Open::Empty => return Ok(Reply::Array(vec![])),
        Open::Other => return Err(WRONGTYPE),
        Open::Found(_) => {}
    }
    let argc = args.len();
    let set = set_ref(cx, key);
    let vdim = set.dim();
    let (vec, dim, vector_args) = if is(&args[2], "ELE") {
        let node = set.node(&args[3]).ok_or(Error::new("ERR element not found in set"))?;
        (set.hnsw.node_vector(node), vdim, 2)
    } else {
        let p = parse_vector(args, 2, false).ok_or(INVALID_VECTOR)?;
        let (mut vec, mut dim) = (p.vec, p.dim);
        if set.proj.is_some() && dim != vdim {
            if dim != set.proj_input {
                return Err(Error::msg(format!(
                    "ERR Input dimension mismatch for projection - got {} but projection expects {}",
                    dim as i32, set.proj_input as i32
                )));
            }
            vec = set.project(&vec);
            dim = vdim;
        }
        let n = if is(&args[2], "FP32") {
            2
        } else if is(&args[2], "VALUES") {
            2 + parse_i64(&args[3]).ok_or(Error::new("ERR invalid vector dimension"))? as usize
        } else {
            return Err(Error::new("ERR vector type must be ELE, FP32 or VALUES"));
        };
        (vec, dim, n)
    };
    if dim != vdim {
        return Err(Error::msg(format!("ERR Vector dimension mismatch - got {} but set has {}", dim as i32, vdim as i32)));
    }

    let (mut withscores, mut withattribs, mut truth) = (false, false, false);
    let mut count = DEFAULT_COUNT;
    let mut ef: i64 = 0;
    let mut epsilon = 2.0f64;
    let mut filter_ef: i64 = 0;
    let mut filter: Option<Expr> = None;
    let mut j = 2 + vector_args;
    while j < argc {
        let opt = &args[j];
        if is(opt, "WITHSCORES") {
            withscores = true;
        } else if is(opt, "WITHATTRIBS") {
            withattribs = true;
        } else if is(opt, "TRUTH") {
            truth = true;
        } else if is(opt, "NOTHREAD") {
        } else if is(opt, "COUNT") && j + 1 < argc {
            count = parse_i64(&args[j + 1]).filter(|&c| c > 0).ok_or(Error::new("ERR invalid COUNT"))?;
            j += 1;
        } else if is(opt, "EPSILON") && j + 1 < argc {
            epsilon = zfloat(&args[j + 1]).filter(|&e| e > 0.0).ok_or(Error::new("ERR invalid EPSILON"))?;
            j += 1;
        } else if is(opt, "EF") && j + 1 < argc {
            ef = parse_i64(&args[j + 1]).filter(|&e| e > 0 && e <= 1_000_000).ok_or(Error::new("ERR invalid EF"))?;
            j += 1;
        } else if is(opt, "FILTER-EF") && j + 1 < argc {
            filter_ef = parse_i64(&args[j + 1]).filter(|&e| e > 0).ok_or(Error::new("ERR invalid FILTER-EF"))?;
            j += 1;
        } else if is(opt, "FILTER") && j + 1 < argc {
            let src = &args[j + 1];
            match Expr::compile(src) {
                Ok(e) => filter = Some(e),
                Err(pos) => {
                    let pos = if pos >= src.len() { 0 } else { pos };
                    let rest = &src[pos..];
                    let rest = &rest[..rest.iter().position(|&c| c == 0).unwrap_or(rest.len())];
                    let near = String::from_utf8_lossy(rest).replace(['\r', '\n'], " ");
                    return Err(Error::msg(format!("ERR syntax error in FILTER expression near: {near}")));
                }
            }
            j += 1;
        } else {
            return Err(Error::new("ERR syntax error in VSIM command"));
        }
        j += 1;
    }
    if filter_ef == 0 {
        filter_ef = count.wrapping_mul(100);
    }

    // VSIM_execute.
    let count = count as u64;
    let mut ef = if ef == 0 { DEFAULT_SEARCH_EF } else { ef as u64 };
    if count > ef {
        ef = count;
    }
    let mut k = set_search(cx, key);
    let set = k.vset();
    ef = ef.min(set.len());
    let q = set.hnsw.probe(&vec);
    let mut accept = |n: &Node| filter.as_ref().is_some_and(|f| n.attrib.as_deref().is_some_and(|a| f.run(a)));
    let found = match (truth, filter.is_some()) {
        (true, true) => set.hnsw.ground_truth(&q, ef as u32, Some(&mut accept)),
        (true, false) => set.hnsw.ground_truth(&q, ef as u32, None),
        (false, true) => set.hnsw.search(&q, ef as u32, Some(&mut accept), filter_ef as u64 as u32),
        (false, false) => set.hnsw.search(&q, ef as u32, None, 0),
    };
    let epsilon = epsilon as f32;
    let mut hits: Vec<Hit> = Vec::new();
    for &(node, d) in found.iter().take(count.min(usize::MAX as u64) as usize) {
        if d / 2.0 > epsilon {
            break;
        }
        let n = set.hnsw.g.n(node);
        let attrib = n.attrib.as_deref().map(Bytes::copy_from_slice);
        hits.push((Bytes::copy_from_slice(&n.item), 1.0 - d as f64 / 2.0, attrib));
    }
    drop(k);

    let attr = |a: Option<Bytes>| a.map_or(Reply::Nil, Reply::Bulk);
    if resp3 && (withscores || withattribs) {
        return Ok(Reply::Map(
            hits.into_iter()
                .map(|(item, score, a)| {
                    let v = match (withscores, withattribs) {
                        (true, true) => Reply::Array(vec![Reply::Double(score), attr(a)]),
                        (true, false) => Reply::Double(score),
                        _ => attr(a),
                    };
                    (Reply::Bulk(item), v)
                })
                .collect(),
        ));
    }
    let mut out = Vec::with_capacity(hits.len() * (1 + withscores as usize + withattribs as usize));
    for (item, score, a) in hits {
        out.push(Reply::Bulk(item));
        if withscores {
            out.push(Reply::Double(score));
        }
        if withattribs {
            out.push(attr(a));
        }
    }
    Ok(Reply::Array(out))
}

/// VDIM key
fn vdim(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    match open_read(cx, &args[1]) {
        Open::Empty => Err(Error::new("ERR key does not exist")),
        Open::Other => Err(WRONGTYPE),
        Open::Found(v) => Ok(Reply::Int(v.dim() as i64)),
    }
}

/// VCARD key
fn vcard(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    match open_read(cx, &args[1]) {
        Open::Empty => Ok(Reply::Int(0)),
        Open::Other => Err(WRONGTYPE),
        Open::Found(v) => Ok(Reply::Int(v.len() as i64)),
    }
}

/// VEMB key element [RAW]
fn vemb(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut raw = false;
    for a in &args[3..] {
        if is(a, "raw") {
            raw = true;
        } else {
            return Err(Error::new("ERR invalid option"));
        }
    }
    let set = match open_read(cx, &args[1]) {
        Open::Empty => return Ok(Reply::Nil),
        Open::Other => return Err(WRONGTYPE),
        Open::Found(v) => v,
    };
    let Some(node) = set.node(&args[2]) else { return Ok(Reply::Nil) };
    if raw {
        let n = set.hnsw.g.n(node);
        let mut out = vec![Reply::status(set.quant_name()), Reply::bulk(n.v.bytes()), Reply::Double(n.l2 as f64)];
        if set.hnsw.g.quant == hnsw::QUANT_Q8 {
            out.push(Reply::Double(n.v.range as f64));
        }
        return Ok(Reply::Array(out));
    }
    Ok(Reply::Array(set.hnsw.node_vector(node).into_iter().map(|x| Reply::Double(x as f64)).collect()))
}

/// VLINKS key element [WITHSCORES]
fn vlinks(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 4 {
        return Err(wrong_arity(cx));
    }
    let withscores = args.len() == 4;
    if withscores && !is(&args[3], "WITHSCORES") {
        return Err(wrong_arity(cx));
    }
    let set = match open_read(cx, &args[1]) {
        Open::Empty => return Ok(Reply::Nil),
        Open::Other => return Err(WRONGTYPE),
        Open::Found(v) => v,
    };
    let Some(node) = set.node(&args[2]) else { return Ok(Reply::Nil) };
    let n = set.hnsw.g.n(node);
    let levels = n
        .layers
        .iter()
        .rev()
        .map(|l| {
            let items = l.links.iter().map(|&t| Reply::bulk(&set.hnsw.g.n(t).item));
            if withscores {
                Reply::Map(
                    items
                        .zip(l.links.iter())
                        .map(|(item, &t)| {
                            let d = set.hnsw.distance(node, t);
                            let similarity = (1.0 - d as f64 / 2.0) as f32;
                            (item, Reply::Double(similarity as f64))
                        })
                        .collect(),
                )
            } else {
                Reply::Array(items.collect())
            }
        })
        .collect();
    Ok(Reply::Array(levels))
}

/// VINFO key
fn vinfo(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let set = match open_read(cx, &args[1]) {
        Open::Empty => return Ok(Reply::NilArray),
        Open::Other => return Err(WRONGTYPE),
        Open::Found(v) => v,
    };
    let g = &set.hnsw.g;
    let field = |name: &str, v: Reply| (Reply::status(name), v);
    Ok(Reply::Map(vec![
        field("quant-type", Reply::status(set.quant_name())),
        field("hnsw-m", Reply::Int(g.m as i64)),
        field("vector-dim", Reply::Int(g.dim as i64)),
        field("projection-input-dim", Reply::Int(set.proj_input as i64)),
        field("size", Reply::Int(g.node_count as i64)),
        field("max-level", Reply::Int(g.max_level as i64)),
        field("attributes-count", Reply::Int(set.numattribs as i64)),
        field("vset-uid", Reply::Int(set.uid as i64)),
        field("hnsw-max-node-uid", Reply::Int(g.last_id as i64)),
    ]))
}

/// VSETATTR key element json
fn vsetattr(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let key = &args[1];
    match open_write(cx, key) {
        Open::Empty => return Ok(Reply::Bool(false)),
        Open::Other => return Err(WRONGTYPE),
        Open::Found(()) => {}
    }
    let mut k = set_mut(cx, key);
    let set = k.vset();
    let Some(node) = set.node(&args[2]) else { return Ok(Reply::Bool(false)) };
    set.set_attrib(node, &args[3]);
    drop(k);
    cx.changes(1);
    Ok(Reply::Bool(true))
}

/// VGETATTR key element
fn vgetattr(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let set = match open_read(cx, &args[1]) {
        Open::Empty => return Ok(Reply::Nil),
        Open::Other => return Err(WRONGTYPE),
        Open::Found(v) => v,
    };
    let Some(node) = set.node(&args[2]) else { return Ok(Reply::Nil) };
    Ok(set.hnsw.g.n(node).attrib.as_deref().map_or(Reply::Nil, Reply::bulk))
}

/// VRANDMEMBER key [count]
fn vrandmember(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 3 {
        return Err(wrong_arity(cx));
    }
    let with_count = args.len() == 3;
    let mut count = 1i64;
    if with_count {
        count = parse_i64(&args[2]).ok_or(Error::new("ERR COUNT value is not an integer"))?;
        if count == 0 {
            return Ok(Reply::Array(vec![]));
        }
        if count == i64::MIN {
            return Err(Error::new("ERR COUNT value is out of range"));
        }
    }
    let empty = if with_count { Reply::Array(vec![]) } else { Reply::Nil };
    let set = match open_read(cx, &args[1]) {
        Open::Empty => return Ok(empty),
        Open::Other => return Err(WRONGTYPE),
        Open::Found(v) => v,
    };
    let size = set.len();
    if size == 0 {
        return Ok(empty);
    }
    let g = &set.hnsw.g;
    let item = |i: u32| Reply::bulk(&g.n(i).item);
    if !with_count {
        let n = set.hnsw.random_node();
        return Ok(if n == hnsw::NIL { Reply::Nil } else { item(n) });
    }
    let dups = count < 0;
    let mut want = count.unsigned_abs();
    if !dups && want > size {
        want = size;
    }
    let mut out = Vec::with_capacity(want.min(1 << 20) as usize);
    if dups {
        for _ in 0..want {
            out.push(item(set.hnsw.random_node()));
        }
    } else if (want as f64) <= size as f64 * 0.2 {
        let mut seen = std::collections::HashSet::new();
        while (out.len() as u64) < want {
            let n = set.hnsw.random_node();
            if seen.insert(n) {
                out.push(item(n));
            }
        }
    } else {
        let mut cur = set.hnsw.random_node();
        while (out.len() as u64) < want {
            if cur == hnsw::NIL {
                cur = g.head;
            }
            out.push(item(cur));
            cur = g.n(cur).next;
        }
    }
    Ok(Reply::Array(out))
}

/// VISMEMBER key element
fn vismember(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    match open_read(cx, &args[1]) {
        Open::Empty => Ok(Reply::Bool(false)),
        Open::Other => Err(WRONGTYPE),
        Open::Found(v) => Ok(Reply::Bool(v.node(&args[2]).is_some())),
    }
}

/// A VRANGE bound: `-`, `+`, `[elem` or `(elem`.
enum Bound<'a> {
    Min,
    Max,
    Incl(&'a [u8]),
    Excl(&'a [u8]),
}

fn range_op(a: &[u8]) -> Option<Bound<'_>> {
    match a {
        [] => None,
        [b'-'] => Some(Bound::Min),
        [b'+'] => Some(Bound::Max),
        [b'[', rest @ ..] if !rest.is_empty() => Some(Bound::Incl(rest)),
        [b'(', rest @ ..] if !rest.is_empty() => Some(Bound::Excl(rest)),
        _ => None,
    }
}

/// VRANGE key start end [count]
fn vrange(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 5 {
        return Err(wrong_arity(cx));
    }
    let mut count = -1i64;
    if args.len() == 5 {
        count = parse_i64(&args[4]).ok_or(Error::new("ERR invalid COUNT value"))?;
    }
    let start = range_op(&args[2]).ok_or(Error::new("ERR invalid start range format"))?;
    let end = range_op(&args[3]).ok_or(Error::new("ERR invalid end range format"))?;
    if matches!(start, Bound::Max) || matches!(end, Bound::Min) {
        return Err(Error::new("ERR '-' can only be used as first argument, '+' only as second"));
    }
    let set = match open_read(cx, &args[1]) {
        Open::Empty => return Ok(Reply::Array(vec![])),
        Open::Other => return Err(WRONGTYPE),
        Open::Found(v) => v,
    };
    use std::ops::Bound as B;
    let lower: B<&[u8]> = match start {
        Bound::Incl(s) => B::Included(s),
        Bound::Excl(s) => B::Excluded(s),
        _ => B::Unbounded,
    };
    let mut out = Vec::new();
    for (ele, _) in set.dict.range::<[u8], _>((lower, B::Unbounded)) {
        if count >= 0 && out.len() as i64 >= count {
            break;
        }
        let inside = match end {
            Bound::Max => true,
            Bound::Incl(e) => &**ele <= e,
            Bound::Excl(e) => &**ele < e,
            Bound::Min => false,
        };
        if !inside {
            break;
        }
        out.push(Reply::bulk(ele));
    }
    Ok(Reply::Array(out))
}
