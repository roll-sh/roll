//! HyperLogLog commands, bit-compatible with Redis' `hyperloglog.c`.
//!
//! An HLL is a plain string value in Redis' exact binary format, so `GET`, `DUMP`/`RESTORE` and
//! replication interchange with real Redis:
//!
//! ```text
//! "HYLL" | encoding (0 dense, 1 sparse) | 3 unused | cached cardinality (8 bytes LE, MSB of the
//! last byte set = stale) | registers
//! ```
//!
//! 16384 6-bit registers, MurmurHash64A (seed 0xadc83b19), the sparse run-length opcodes
//! (`ZERO`, `XZERO`, `VAL`) promoted to dense past `hll-sparse-max-bytes`, and the Ertl
//! estimator used since Redis 5.

use super::*;
use crate::cmd;
use crate::notify;
use crate::util::Rng;
use crate::value::{Kind, Str, Value};

pub static COMMANDS: &[Command] = &[
    cmd!("pfadd", -2, WRITE | DENYOOM | FAST, 1, 1, 1, pfadd, HyperLogLog),
    cmd!("pfcount", -2, READONLY, 1, -1, 1, pfcount, HyperLogLog),
    cmd!("pfmerge", -2, WRITE | DENYOOM, 1, -1, 1, pfmerge, HyperLogLog),
    cmd!("pfdebug", 3, WRITE | DENYOOM | ADMIN, 2, 2, 1, pfdebug, HyperLogLog),
    cmd!("pfselftest", 1, ADMIN, 0, 0, 0, pfselftest, HyperLogLog),
];

pub const HLL_WRONGTYPE: Error = Error::new("WRONGTYPE Key is not a valid HyperLogLog string value.");
pub const INVALIDOBJ: Error = Error::new("INVALIDOBJ Corrupted HLL object detected");

const P: u32 = 14;
const Q: usize = 64 - P as usize;
pub const REGISTERS: usize = 1 << P;
const BITS: usize = 6;
const REGISTER_MAX: u32 = (1 << BITS) - 1;
pub const HDR: usize = 16;
pub const DENSE_SIZE: usize = HDR + (REGISTERS * BITS).div_ceil(8);
const DENSE: u8 = 0;
const SPARSE: u8 = 1;
const VAL_MAX_VALUE: u8 = 32;
const VAL_MAX_LEN: usize = 4;
const ZERO_MAX_LEN: usize = 64;
const XZERO_MAX_LEN: usize = 16384;
const ALPHA_INF: f64 = 0.721_347_520_444_481_7;

/// MurmurHash64A, as in Redis (little-endian reads).
pub fn murmur64a(key: &[u8], seed: u64) -> u64 {
    const M: u64 = 0xc6a4_a793_5bd1_e995;
    const R: u32 = 47;
    let len = key.len();
    let mut h = seed ^ (len as u64).wrapping_mul(M);
    let (chunks, tail) = key.as_chunks::<8>();
    for c in chunks {
        let mut k = u64::from_le_bytes(*c);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h ^= k;
        h = h.wrapping_mul(M);
    }
    if !tail.is_empty() {
        for (i, &b) in tail.iter().enumerate().rev() {
            h ^= (b as u64) << (8 * i);
        }
        h = h.wrapping_mul(M);
    }
    h ^= h >> R;
    h = h.wrapping_mul(M);
    h ^= h >> R;
    h
}

/// Register index and run length of "000…1" for an element (`hllPatLen`).
fn pat_len(ele: &[u8]) -> (usize, u8) {
    let mut hash = murmur64a(ele, 0xadc8_3b19);
    let index = (hash & (REGISTERS as u64 - 1)) as usize;
    hash >>= P;
    hash |= 1u64 << Q;
    (index, (hash.trailing_zeros() + 1) as u8)
}

// ---- header ----

/// `isHLLObjectOrReply`'s checks on a string value.
pub fn is_valid(s: &[u8]) -> bool {
    s.len() >= HDR && &s[..4] == b"HYLL" && s[4] <= SPARSE && (s[4] != DENSE || s.len() == DENSE_SIZE)
}

/// A new, empty HLL: sparse, one XZERO opcode covering every register, cached cardinality 0.
pub fn new_sparse() -> Str {
    let mut s = Str::from_slice(b"HYLL\x01\x00\x00\x00");
    s.extend_from_slice(&[0; 8]);
    let mut left = REGISTERS;
    while left > 0 {
        let n = left.min(XZERO_MAX_LEN);
        let l = n - 1;
        s.push(((l >> 8) as u8) | 0x40);
        s.push((l & 0xff) as u8);
        left -= n;
    }
    s
}

fn invalidate_cache(s: &mut [u8]) {
    s[15] |= 1 << 7;
}

fn cache_valid(s: &[u8]) -> bool {
    s[15] & (1 << 7) == 0
}

fn cached_card(s: &[u8]) -> u64 {
    u64::from_le_bytes(s[8..16].try_into().unwrap())
}

fn set_cached_card(s: &mut [u8], card: u64) {
    s[8..16].copy_from_slice(&card.to_le_bytes());
}

// ---- dense ----

#[inline]
fn dense_get(regs: &[u8], i: usize) -> u8 {
    let byte = i * BITS / 8;
    let fb = (i * BITS) & 7;
    let fb8 = 8 - fb;
    let b0 = regs[byte] as u32;
    let b1 = regs.get(byte + 1).copied().unwrap_or(0) as u32;
    (((b0 >> fb) | (b1 << fb8)) & REGISTER_MAX) as u8
}

#[inline]
fn dense_put(regs: &mut [u8], i: usize, v: u8) {
    let byte = i * BITS / 8;
    let fb = (i * BITS) & 7;
    let fb8 = 8 - fb;
    let v = v as u32;
    regs[byte] &= !((REGISTER_MAX << fb) as u8);
    regs[byte] |= (v << fb) as u8;
    if byte + 1 < regs.len() {
        regs[byte + 1] &= !((REGISTER_MAX >> fb8) as u8);
        regs[byte + 1] |= (v >> fb8) as u8;
    }
}

/// Set register `i` to `count` if larger (`hllDenseSet`).
fn dense_set(regs: &mut [u8], i: usize, count: u8) -> bool {
    if count > dense_get(regs, i) {
        dense_put(regs, i, count);
        true
    } else {
        false
    }
}

// ---- sparse ----

#[inline]
fn is_zero(op: u8) -> bool {
    op & 0xc0 == 0
}
#[inline]
fn is_xzero(op: u8) -> bool {
    op & 0xc0 == 0x40
}
#[inline]
fn is_val(op: u8) -> bool {
    op & 0x80 != 0
}
#[inline]
fn zero_len(op: u8) -> usize {
    (op & 0x3f) as usize + 1
}
/// XZERO length; the second byte past the end reads as 0 (Redis reads the sds terminator).
#[inline]
fn xzero_len(s: &[u8], p: usize) -> usize {
    ((((s[p] & 0x3f) as usize) << 8) | s.get(p + 1).copied().unwrap_or(0) as usize) + 1
}
#[inline]
fn val_value(op: u8) -> u8 {
    ((op >> 2) & 0x1f) + 1
}
#[inline]
fn val_len(op: u8) -> usize {
    (op & 0x3) as usize + 1
}
#[inline]
fn val_op(val: u8, len: usize) -> u8 {
    (((val - 1) << 2) | (len as u8 - 1)) | 0x80
}
fn push_zero(seq: &mut Vec<u8>, len: usize) {
    if len > ZERO_MAX_LEN {
        let l = len - 1;
        seq.push(((l >> 8) as u8) | 0x40);
        seq.push((l & 0xff) as u8);
    } else {
        seq.push((len - 1) as u8);
    }
}

/// Convert a sparse HLL to dense in place (`hllSparseToDense`). False if the sparse data is
/// corrupt (then `s` is unchanged).
pub fn sparse_to_dense(s: &mut Str) -> bool {
    if s[4] == DENSE {
        return true;
    }
    let mut d = Str::from_elem(0, DENSE_SIZE);
    d[..HDR].copy_from_slice(&s[..HDR]);
    d[4] = DENSE;
    let regs = &mut d[HDR..];
    let (mut idx, mut p) = (0usize, HDR);
    while p < s.len() {
        let op = s[p];
        if is_zero(op) {
            idx += zero_len(op);
            p += 1;
        } else if is_xzero(op) {
            idx += xzero_len(s, p);
            p += 2;
        } else {
            let (runlen, val) = (val_len(op), val_value(op));
            if runlen + idx > REGISTERS {
                break;
            }
            for _ in 0..runlen {
                dense_put(regs, idx, val);
                idx += 1;
            }
            p += 1;
        }
    }
    if idx != REGISTERS {
        return false;
    }
    *s = d;
    true
}

/// Promote to dense and set the register (the result is always an update).
fn promote_and_set(s: &mut Str, index: usize, count: u8) -> i32 {
    if !sparse_to_dense(s) {
        return -1;
    }
    dense_set(&mut s[HDR..], index, count);
    1
}

/// `hllSparseSet`: 1 if a register changed, 0 if not, -1 if the data is corrupt.
fn sparse_set(s: &mut Str, index: usize, count: u8, max_bytes: usize) -> i32 {
    if count > VAL_MAX_VALUE {
        return promote_and_set(s, index, count);
    }
    // Step 1: find the opcode covering `index`.
    let end = s.len();
    let (mut p, mut first, mut span) = (HDR, 0usize, 0usize);
    let mut prev: Option<usize> = None;
    while p < end {
        let op = s[p];
        let mut oplen = 1;
        if is_zero(op) {
            span = zero_len(op);
        } else if is_val(op) {
            span = val_len(op);
        } else {
            span = xzero_len(s, p);
            oplen = 2;
        }
        if index < first + span {
            break;
        }
        prev = Some(p);
        p += oplen;
        first += span;
    }
    if span == 0 || p >= end {
        return -1;
    }
    let op = s[p];
    let (zero, xzero, val) = (is_zero(op), is_xzero(op), is_val(op));
    let runlen = span;

    let mut updated = false;
    // Step 2: trivial in-place updates.
    if val {
        if val_value(op) >= count {
            return 0;
        }
        if runlen == 1 {
            s[p] = val_op(count, 1);
            updated = true;
        }
    }
    if !updated && zero && runlen == 1 {
        s[p] = val_op(count, 1);
        updated = true;
    }
    if !updated {
        // General case: split the opcode into up to three.
        let last = first + span - 1;
        let mut seq = Vec::with_capacity(5);
        if zero || xzero {
            if index != first {
                push_zero(&mut seq, index - first);
            }
            seq.push(val_op(count, 1));
            if index != last {
                push_zero(&mut seq, last - index);
            }
        } else {
            let cur = val_value(op);
            if index != first {
                seq.push(val_op(cur, index - first));
            }
            seq.push(val_op(count, 1));
            if index != last {
                seq.push(val_op(cur, last - index));
            }
        }
        // Step 3: substitute it.
        let oldlen = if xzero { 2 } else { 1 };
        if seq.len() > oldlen && s.len() + (seq.len() - oldlen) > max_bytes {
            return promote_and_set(s, index, count);
        }
        // A truncated XZERO at the very end: Redis writes the sequence but the string only grows
        // by the length difference, cutting it short.
        let new_len = s.len() + seq.len() - oldlen;
        let tail: Str = Str::from_slice(&s[(p + oldlen).min(s.len())..]);
        s.truncate(p);
        s.extend_from_slice(&seq);
        s.extend_from_slice(&tail);
        s.truncate(new_len);
    }
    // Step 4: merge adjacent VAL opcodes with the same value, scanning up to 5 opcodes from the
    // previous one.
    let mut q = prev.unwrap_or(HDR);
    let mut end = s.len();
    let mut scanlen = 5;
    while q < end {
        if scanlen == 0 {
            break;
        }
        scanlen -= 1;
        let op = s[q];
        if is_xzero(op) {
            q += 2;
            continue;
        } else if is_zero(op) {
            q += 1;
            continue;
        }
        if q + 1 < end && is_val(s[q + 1]) {
            let (v1, v2) = (val_value(op), val_value(s[q + 1]));
            if v1 == v2 {
                let len = val_len(op) + val_len(s[q + 1]);
                if len <= VAL_MAX_LEN {
                    s[q + 1] = val_op(v1, len);
                    s.remove(q);
                    end -= 1;
                    continue;
                }
            }
        }
        q += 1;
    }
    invalidate_cache(s);
    1
}

/// `hllAdd`: 1 if a register changed, 0 if not, -1 for a corrupt HLL.
fn hll_add(s: &mut Str, ele: &[u8], max_bytes: usize) -> i32 {
    let (index, count) = pat_len(ele);
    match s[4] {
        DENSE => dense_set(&mut s[HDR..], index, count) as i32,
        SPARSE => sparse_set(s, index, count, max_bytes),
        _ => -1,
    }
}

/// Register histogram (`hllDenseRegHisto` / `hllSparseRegHisto`). Sets `invalid` for a corrupt
/// sparse HLL.
fn histogram(s: &[u8], invalid: &mut bool) -> [u32; 64] {
    let mut h = [0u32; 64];
    if s[4] == DENSE {
        let regs = &s[HDR..];
        for i in 0..REGISTERS {
            h[dense_get(regs, i) as usize] += 1;
        }
        return h;
    }
    let (mut idx, mut p) = (0usize, HDR);
    while p < s.len() {
        let op = s[p];
        if is_zero(op) {
            let n = zero_len(op);
            idx += n;
            h[0] += n as u32;
            p += 1;
        } else if is_xzero(op) {
            let n = xzero_len(s, p);
            idx += n;
            h[0] += n as u32;
            p += 2;
        } else {
            let n = val_len(op);
            idx += n;
            h[val_value(op) as usize] += n as u32;
            p += 1;
        }
    }
    if idx != REGISTERS {
        *invalid = true;
    }
    h
}

fn hll_sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let mut y = 1.0;
    let mut z = x;
    loop {
        x *= x;
        let zp = z;
        z += x * y;
        y += y;
        if zp == z {
            return z;
        }
    }
}

fn hll_tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let mut y = 1.0;
    let mut z = 1.0 - x;
    loop {
        x = x.sqrt();
        let zp = z;
        y *= 0.5;
        let d = 1.0 - x;
        z -= d * d * y;
        if zp == z {
            return z / 3.0;
        }
    }
}

/// The Ertl estimator over a register histogram (`hllCount`).
fn estimate(h: &[u32; 64]) -> u64 {
    let m = REGISTERS as f64;
    let mut z = m * hll_tau((m - h[Q + 1] as f64) / m);
    for j in (1..=Q).rev() {
        z += h[j] as f64;
        z *= 0.5;
    }
    z += m * hll_sigma(h[0] as f64 / m);
    (ALPHA_INF * m * m / z).round() as u64
}

/// Raw registers (one byte each) for merges: multi-key PFCOUNT, PFMERGE, SUNIONCARD APPROX.
pub struct Registers(Box<[u8; REGISTERS]>);

impl Default for Registers {
    fn default() -> Self {
        Self::new()
    }
}

impl Registers {
    pub fn new() -> Self {
        Self(Box::new([0; REGISTERS]))
    }

    /// Count an element.
    pub fn add(&mut self, ele: &[u8]) {
        let (i, c) = pat_len(ele);
        if c > self.0[i] {
            self.0[i] = c;
        }
    }

    /// `hllMerge`: max with a (validated) HLL string. False if it is corrupt.
    pub fn merge(&mut self, s: &[u8]) -> bool {
        let max = &mut self.0;
        if s[4] == DENSE {
            let regs = &s[HDR..];
            for (i, m) in max.iter_mut().enumerate() {
                let v = dense_get(regs, i);
                if v > *m {
                    *m = v;
                }
            }
            return true;
        }
        let (mut i, mut p) = (0usize, HDR);
        while p < s.len() {
            let op = s[p];
            if is_zero(op) {
                i += zero_len(op);
                p += 1;
            } else if is_xzero(op) {
                i += xzero_len(s, p);
                p += 2;
            } else {
                let (n, v) = (val_len(op), val_value(op));
                if n + i > REGISTERS {
                    break;
                }
                for m in &mut max[i..i + n] {
                    if v > *m {
                        *m = v;
                    }
                }
                i += n;
                p += 1;
            }
        }
        i == REGISTERS
    }

    pub fn count(&self) -> u64 {
        let mut h = [0u32; 64];
        for &r in self.0.iter() {
            h[r as usize] += 1;
        }
        estimate(&h)
    }
}

/// Validate a looked-up value as an HLL.
fn check(v: &Value) -> Result<&Str, Error> {
    match v {
        Value::Str(s) if is_valid(s) => Ok(s),
        Value::Str(_) => Err(HLL_WRONGTYPE),
        _ => Err(WRONGTYPE),
    }
}

/// Is the key an HLL? `Ok(false)` if missing; WRONGTYPE errors otherwise.
fn check_key(cx: &mut Cx, key: &[u8]) -> Result<bool, Error> {
    match cx.peek(key) {
        None => Ok(false),
        Some(e) => check(&e.value).map(|_| true),
    }
}

pub fn pfadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let key = &args[1];
    let max_bytes = cx.cfg().hll_sparse_max_bytes;
    let exists = check_key(cx, key)?;
    let mut e = cx.get_or_create(key, Kind::String)?;
    let res = {
        let s = e.str();
        let mut updated = 0;
        if !exists {
            *s = new_sparse();
            updated += 1;
        }
        let mut corrupt = false;
        for ele in &args[2..] {
            match hll_add(s, ele, max_bytes) {
                1 => updated += 1,
                -1 => {
                    corrupt = true;
                    break;
                }
                _ => {}
            }
        }
        if !corrupt && updated > 0 {
            invalidate_cache(s);
        }
        if corrupt { Err(INVALIDOBJ) } else { Ok(updated > 0) }
    };
    match res {
        Ok(true) => {
            e.notify(notify::STRING, "pfadd");
            Ok(Reply::Int(1))
        }
        Ok(false) => {
            e.unchanged();
            Ok(Reply::Int(0))
        }
        Err(err) => {
            e.unchanged();
            Err(err)
        }
    }
}

pub fn pfcount(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 2 {
        let mut regs = Registers::new();
        for k in &args[1..] {
            let Some(v) = cx.get(k) else { continue };
            if !regs.merge(check(v)?) {
                return Err(INVALIDOBJ);
            }
        }
        return Ok(Reply::Int(regs.count() as i64));
    }
    let key = &args[1];
    let (card, fresh) = {
        let Some(v) = cx.get(key) else { return Ok(Reply::Int(0)) };
        let s = check(v)?;
        if cache_valid(s) {
            (cached_card(s), false)
        } else {
            let mut invalid = false;
            let h = histogram(s, &mut invalid);
            if invalid {
                return Err(INVALIDOBJ);
            }
            (estimate(&h), true)
        }
    };
    if fresh {
        // Caching the cardinality is a write (propagated, invalidates WATCH), like in Redis.
        let mut e = cx.get_mut(key).expect("looked up above");
        set_cached_card(e.str(), card);
    }
    Ok(Reply::Int(card as i64))
}

pub fn pfmerge(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let max_bytes = cx.cfg().hll_sparse_max_bytes;
    let mut regs = Registers::new();
    let mut use_dense = false;
    for k in &args[1..] {
        let Some(v) = cx.get(k) else { continue };
        let s = check(v)?;
        if s[4] == DENSE {
            use_dense = true;
        }
        if !regs.merge(s) {
            return Err(INVALIDOBJ);
        }
    }
    let dst = &args[1];
    let exists = cx.peek(dst).is_some();
    let mut e = cx.get_or_create(dst, Kind::String)?;
    let ok = {
        let s = e.str();
        if !exists {
            *s = new_sparse();
        }
        if use_dense {
            if sparse_to_dense(s) {
                let d = &mut s[HDR..];
                for (i, &v) in regs.0.iter().enumerate() {
                    dense_put(d, i, v);
                }
                true
            } else {
                false
            }
        } else {
            for (j, &v) in regs.0.iter().enumerate() {
                if v == 0 {
                    continue;
                }
                if s[4] == DENSE {
                    dense_set(&mut s[HDR..], j, v);
                } else {
                    sparse_set(s, j, v, max_bytes);
                }
            }
            true
        }
    };
    if !ok {
        e.unchanged();
        return Err(INVALIDOBJ);
    }
    invalidate_cache(e.str());
    e.notify(notify::STRING, "pfadd");
    Ok(Reply::Ok)
}

/// PFDEBUG GETREG|DECODE|ENCODING|TODENSE key
pub fn pfdebug(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (sub, key) = (&args[1], &args[2]);
    let Some(entry) = cx.peek(key) else { return Err(Error::new("ERR The specified key does not exist")) };
    let s = check(&entry.value)?;
    if eq(sub, "decode") {
        if s[4] != SPARSE {
            return Err(Error::new("ERR HLL encoding is not sparse"));
        }
        let mut out = Vec::new();
        let mut p = HDR;
        while p < s.len() {
            let op = s[p];
            if is_zero(op) {
                out.push(format!("z:{}", zero_len(op)));
                p += 1;
            } else if is_xzero(op) {
                out.push(format!("Z:{}", xzero_len(s, p)));
                p += 2;
            } else {
                out.push(format!("v:{},{}", val_value(op), val_len(op)));
                p += 1;
            }
        }
        return Ok(Reply::bulk_str(out.join(" ")));
    }
    if eq(sub, "encoding") {
        return Ok(Reply::status(if s[4] == DENSE { "dense" } else { "sparse" }));
    }
    let getreg = eq(sub, "getreg");
    if !getreg && !eq(sub, "todense") {
        return Err(Error::msg(format!("ERR Unknown PFDEBUG subcommand '{}'", String::from_utf8_lossy(sub))));
    }
    let mut e = cx.get_mut(key).expect("looked up above");
    let st = e.str();
    let converted = st[4] == SPARSE;
    if converted && !sparse_to_dense(st) {
        e.unchanged();
        return Err(INVALIDOBJ);
    }
    let reply = if getreg {
        let regs = &st[HDR..];
        Reply::Array((0..REGISTERS).map(|i| Reply::Int(dense_get(regs, i) as i64)).collect())
    } else {
        Reply::Int(converted as i64)
    };
    if !converted {
        e.unchanged();
    }
    Ok(reply)
}

/// PFSELFTEST: check register packing and that sparse and dense representations agree.
pub fn pfselftest(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let fail = |m: &str| Err(Error::msg(format!("ERR TESTFAILED {m}")));
    let mut rng = Rng::seeded(cx.eng.rng.next_u64());
    // Register access: random values written then read back.
    let mut regs = vec![0u8; DENSE_SIZE - HDR];
    let mut expect = vec![0u8; REGISTERS];
    for _ in 0..4 {
        for (i, e) in expect.iter_mut().enumerate() {
            *e = (rng.next_u64() & REGISTER_MAX as u64) as u8;
            dense_put(&mut regs, i, *e);
        }
        for (i, &e) in expect.iter().enumerate() {
            if dense_get(&regs, i) != e {
                return fail("Register error");
            }
        }
    }
    // Sparse and dense must agree on every register and on the estimate.
    let mut sparse = new_sparse();
    let mut dense = new_sparse();
    sparse_to_dense(&mut dense);
    for i in 0..20_000u32 {
        let ele = i.to_le_bytes();
        hll_add(&mut sparse, &ele, usize::MAX);
        hll_add(&mut dense, &ele, usize::MAX);
    }
    let (mut a, mut b) = (false, false);
    if histogram(&sparse, &mut a) != histogram(&dense, &mut b) || a || b {
        return fail("Sparse and dense representations disagree");
    }
    let est = estimate(&histogram(&dense, &mut a)) as f64;
    if (est - 20_000.0).abs() / 20_000.0 > 0.05 {
        return fail("Approximation error too large");
    }
    Ok(Reply::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn murmur_known() {
        // The empty HLL is the sparse opcode XZERO:16384.
        assert_eq!(&new_sparse()[..], b"HYLL\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x7f\xff");
        assert_eq!(murmur64a(b"", 0), 0);
    }

    #[test]
    fn matches_redis_bytes() {
        // PFADD h a b c on redis:8 gives these bytes.
        let mut s = new_sparse();
        for e in [b"a", b"b", b"c"] {
            assert_eq!(hll_add(&mut s, e, 3000), 1);
        }
        invalidate_cache(&mut s);
        assert_eq!(&s[..], b"HYLL\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x80`\xf3\x80P\xb1\x84K\xfb\x80BZ");
        let mut inv = false;
        assert_eq!(estimate(&histogram(&s, &mut inv)), 3);
        assert!(!inv);
    }

    #[test]
    fn sparse_dense_agree() {
        let mut s = new_sparse();
        let mut d = new_sparse();
        assert!(sparse_to_dense(&mut d));
        for i in 0..5000 {
            let e = format!("e{i}");
            hll_add(&mut s, e.as_bytes(), 1 << 20);
            hll_add(&mut d, e.as_bytes(), 1 << 20);
        }
        let (mut a, mut b) = (false, false);
        assert_eq!(histogram(&s, &mut a), histogram(&d, &mut b));
        let est = estimate(&histogram(&d, &mut a));
        assert!((4800..5200).contains(&est), "{est}");
    }
}
