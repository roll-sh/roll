//! Bitmap commands (string values addressed by bit): SETBIT, GETBIT, BITCOUNT, BITPOS, BITOP,
//! BITFIELD and BITFIELD_RO, following Redis' `bitops.c`.

use super::*;
use crate::cmd;
use crate::engine::Mode;
use crate::notify;
use crate::util::parse_i64;
use crate::value::{Entry, Kind, Str, Value};

pub static COMMANDS: &[Command] = &[
    cmd!("setbit", 4, WRITE | DENYOOM, 1, 1, 1, setbit, Bitmap),
    cmd!("getbit", 3, READONLY | FAST, 1, 1, 1, getbit, Bitmap),
    cmd!("bitcount", -2, READONLY, 1, 1, 1, bitcount, Bitmap),
    cmd!("bitpos", -3, READONLY, 1, 1, 1, bitpos, Bitmap),
    cmd!("bitop", -4, WRITE | DENYOOM, 2, -1, 1, bitop, Bitmap),
    cmd!("bitfield", -2, WRITE | DENYOOM, 1, 1, 1, bitfield, Bitmap),
    cmd!("bitfield_ro", -2, READONLY | FAST, 1, 1, 1, bitfield_ro, Bitmap),
];

const OFFSET_ERR: Error = Error::new("ERR bit offset is not an integer or out of range");
const BIT_ERR: Error = Error::new("ERR bit is not an integer or out of range");
const TYPE_ERR: Error =
    Error::new("ERR Invalid bitfield type. Use something like i16 u8. Note that u64 is not supported but i64 is.");

/// `getBitOffsetFromArgument`: a bit offset, or with `#N` (BITFIELD) N times the field width.
/// Offsets beyond `proto-max-bulk-len` bytes are refused except for replicated writes.
fn bit_offset(cx: &Cx, a: &[u8], hash: bool, bits: u32) -> Result<u64, Error> {
    let use_hash = hash && bits > 0 && a.first() == Some(&b'#');
    let mut v = parse_i64(&a[use_hash as usize..]).ok_or(OFFSET_ERR)?;
    if use_hash {
        v = v.checked_mul(bits as i64).ok_or(OFFSET_ERR)?;
    }
    if v < 0 || (cx.mode != Mode::Replay && (v as u64 >> 3) >= cx.cfg().proto_max_bulk_len) {
        return Err(OFFSET_ERR);
    }
    Ok(v as u64)
}

/// Zero-extend a string to `len` bytes, growing the allocation like `sdsMakeRoomFor` (double
/// below 1 MB, then +1 MB) so bit-by-bit growth is amortised without power-of-two overshoot.
fn grow_zero(s: &mut Str, len: usize) -> bool {
    if s.len() >= len {
        return false;
    }
    if len > s.capacity() {
        const MB: usize = 1024 * 1024;
        let target = if len < MB { len * 2 } else { len + MB };
        s.reserve_exact(target - s.len());
    }
    s.resize(len, 0);
    true
}

pub fn setbit(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let off = bit_offset(cx, &args[2], false, 0)?;
    let on = parse_i64(&args[3]).ok_or(BIT_ERR)?;
    if on & !1 != 0 {
        return Err(BIT_ERR);
    }
    let byte = (off >> 3) as usize;
    let old_len = str_len(cx, &args[1]);
    let mut e = cx.get_or_create(&args[1], Kind::String)?;
    let s = if old_len.is_some() { e.str_in_place(byte + 1) } else { e.str_new_raw() };
    let grown = grow_zero(s, byte + 1);
    let new_len = s.len();
    let bit = 7 - (off & 7) as u32;
    let old = (s[byte] >> bit) & 1;
    if grown || old as i64 != on {
        s[byte] = (s[byte] & !(1 << bit)) | ((on as u8) << bit);
        e.notify(notify::STRING, "setbit");
        drop(e);
        // An existing empty string grown keeps its keysizes bin in Redis (`strOldSize > 0`).
        if old_len == Some(0) {
            let db = cx.db_index();
            cx.eng.dbs[db].keysizes_skip_move(0, new_len);
        }
    } else {
        e.unchanged();
    }
    Ok(Reply::Int(old as i64))
}

pub fn getbit(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let off = bit_offset(cx, &args[2], false, 0)?;
    let Some(s) = cx.get_str(&args[1])? else { return Ok(Reply::Int(0)) };
    let byte = (off >> 3) as usize;
    let bit = 7 - (off & 7) as u32;
    Ok(Reply::Int(s.get(byte).map_or(0, |b| (b >> bit) & 1) as i64))
}

/// `BYTE | BIT` unit argument.
fn parse_unit(a: &[u8]) -> Result<bool, Error> {
    if eq(a, "bit") {
        Ok(true)
    } else if eq(a, "byte") {
        Ok(false)
    } else {
        Err(SYNTAX)
    }
}

/// Normalise a `[start, end]` range over `strlen` bytes (in bits when `isbit`). Returns the byte
/// range and the masks of out-of-range bits in the first and last bytes.
fn bit_range(mut start: i64, mut end: i64, strlen: i64, isbit: bool) -> (i64, i64, u8, u8) {
    let mut totlen = strlen;
    if isbit {
        totlen <<= 3;
    }
    if start < 0 {
        start = totlen.saturating_add(start);
    }
    if end < 0 {
        end = totlen.saturating_add(end);
    }
    start = start.max(0);
    end = end.max(0);
    if end >= totlen {
        end = totlen - 1;
    }
    let (mut first_mask, mut last_mask) = (0u8, 0u8);
    if isbit && start <= end {
        first_mask = !((1u32 << (8 - (start & 7))) - 1) as u8;
        last_mask = ((1u32 << (7 - (end & 7))) - 1) as u8;
        start >>= 3;
        end >>= 3;
    }
    (start, end, first_mask, last_mask)
}

fn popcount(s: &[u8]) -> i64 {
    let (chunks, rest) = s.as_chunks::<8>();
    let mut n: u64 = 0;
    for c in chunks {
        n += u64::from_ne_bytes(*c).count_ones() as u64;
    }
    n += rest.iter().map(|b| b.count_ones() as u64).sum::<u64>();
    n as i64
}

/// BITCOUNT key [start end [BYTE | BIT]]
pub fn bitcount(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let ranged = match args.len() {
        2 => false,
        4 | 5 => true,
        _ => return Err(SYNTAX),
    };
    let (mut start, mut end, mut isbit) = (0i64, 0i64, false);
    if ranged {
        start = int(&args[2])?;
        end = int(&args[3])?;
        if args.len() == 5 {
            isbit = parse_unit(&args[4])?;
        }
    }
    let Some(s) = cx.get_str(&args[1])? else { return Ok(Reply::Int(0)) };
    let strlen = s.len() as i64;
    let (mut fm, mut lm) = (0u8, 0u8);
    if ranged {
        if start < 0 && end < 0 && start > end {
            return Ok(Reply::Int(0));
        }
        (start, end, fm, lm) = bit_range(start, end, strlen, isbit);
    } else {
        start = 0;
        end = strlen - 1;
    }
    if start > end {
        return Ok(Reply::Int(0));
    }
    let (a, z) = (start as usize, end as usize);
    let mut count = popcount(&s[a..=z]);
    if fm != 0 {
        count -= (s[a] & fm).count_ones() as i64;
    }
    if lm != 0 {
        count -= (s[z] & lm).count_ones() as i64;
    }
    Ok(Reply::Int(count))
}

/// `redisBitpos`: first bit equal to `bit`; not found: -1 for 1, `len*8` for 0 (the string is
/// considered zero-padded on the right).
fn first_bit(s: &[u8], bit: bool) -> i64 {
    for (i, &b) in s.iter().enumerate() {
        let v = if bit { b } else { !b };
        if v != 0 {
            return (i * 8) as i64 + v.leading_zeros() as i64;
        }
    }
    if bit { -1 } else { (s.len() * 8) as i64 }
}

/// BITPOS key bit [start [end [BYTE | BIT]]]
pub fn bitpos(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let bit = int(&args[2])?;
    if bit != 0 && bit != 1 {
        return Err(Error::new("ERR The bit argument must be 1 or 0."));
    }
    let bit = bit == 1;
    let n = args.len();
    let (mut start, mut end, mut isbit, mut end_given) = (0i64, 0i64, false, false);
    match n {
        3 => {}
        4..=6 => {
            start = int(&args[3])?;
            if n == 6 {
                isbit = parse_unit(&args[5])?;
            }
            if n >= 5 {
                end = int(&args[4])?;
                end_given = true;
            }
        }
        _ => return Err(SYNTAX),
    }
    let Some(s) = cx.get_str(&args[1])? else { return Ok(Reply::Int(if bit { -1 } else { 0 })) };
    let strlen = s.len() as i64;
    let (mut fm, mut lm) = (0u8, 0u8);
    if n >= 4 {
        if n < 5 {
            end = strlen - 1;
        }
        (start, end, fm, lm) = bit_range(start, end, strlen, isbit);
    } else {
        start = 0;
        end = strlen - 1;
    }
    if start > end {
        return Ok(Reply::Int(-1));
    }
    let p = &s[..];
    let mut bytes = end - start + 1;
    let pos = 'r: {
        if fm != 0 {
            let mut tmp = if bit { p[start as usize] & !fm } else { p[start as usize] | fm };
            if lm != 0 && bytes == 1 {
                tmp = if bit { tmp & !lm } else { tmp | lm };
            }
            let pos = first_bit(&[tmp], bit);
            if bytes == 1 || (pos != -1 && pos != 8) {
                break 'r pos;
            }
            start += 1;
            bytes -= 1;
        }
        let cur = bytes - (lm != 0) as i64;
        if cur > 0 {
            let pos = first_bit(&p[start as usize..(start + cur) as usize], bit);
            if bytes == cur || (pos != -1 && pos != cur << 3) {
                break 'r pos;
            }
            start += cur;
            bytes -= cur;
        }
        let tmp = if bit { p[end as usize] & !lm } else { p[end as usize] | lm };
        first_bit(&[tmp], bit)
    };
    if end_given && !bit && pos == bytes << 3 {
        return Ok(Reply::Int(-1));
    }
    Ok(Reply::Int(if pos != -1 { pos + (start << 3) } else { pos }))
}

#[derive(Clone, Copy, PartialEq)]
enum BitOp {
    And,
    Or,
    Xor,
    Not,
    Diff,
    Diff1,
    AndOr,
    One,
}

/// BITOP AND|OR|XOR|NOT|DIFF|DIFF1|ANDOR|ONE destkey key [key …]
pub fn bitop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let o = &args[1];
    let op = if eq(o, "and") {
        BitOp::And
    } else if eq(o, "or") {
        BitOp::Or
    } else if eq(o, "xor") {
        BitOp::Xor
    } else if eq(o, "not") {
        BitOp::Not
    } else if eq(o, "diff") {
        BitOp::Diff
    } else if eq(o, "diff1") {
        BitOp::Diff1
    } else if eq(o, "andor") {
        BitOp::AndOr
    } else if eq(o, "one") {
        BitOp::One
    } else {
        return Err(SYNTAX);
    };
    let keys = &args[3..];
    if op == BitOp::Not && keys.len() != 1 {
        return Err(Error::new("ERR BITOP NOT must be called with a single source key."));
    }
    if matches!(op, BitOp::Diff | BitOp::Diff1 | BitOp::AndOr) && keys.len() < 2 {
        let name = String::from_utf8_lossy(o).to_uppercase();
        return Err(Error::msg(format!("ERR BITOP {name} must be called with at least two source keys.")));
    }
    let mut exists = Vec::with_capacity(keys.len());
    for k in keys {
        exists.push(cx.get_str(k)?.is_some());
    }
    let res: Vec<u8> = {
        let db = &cx.eng.dbs[cx.client.db];
        let srcs: Vec<&[u8]> = keys
            .iter()
            .zip(&exists)
            .map(|(k, &ok)| match db.keys.get(k) {
                Some(Entry { value: Value::Str(s), .. }) if ok => &s[..],
                _ => &[][..],
            })
            .collect();
        let maxlen = srcs.iter().map(|s| s.len()).max().unwrap_or(0);
        let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
        let mut out = vec![0u8; maxlen];
        for (i, o) in out.iter_mut().enumerate() {
            let x = at(srcs[0], i);
            let rest = || srcs[1..].iter().fold(0u8, |acc, s| acc | at(s, i));
            *o = match op {
                BitOp::And => srcs[1..].iter().fold(x, |acc, s| acc & at(s, i)),
                BitOp::Or => srcs[1..].iter().fold(x, |acc, s| acc | at(s, i)),
                BitOp::Xor => srcs[1..].iter().fold(x, |acc, s| acc ^ at(s, i)),
                BitOp::Not => !x,
                BitOp::Diff => x & !rest(),
                BitOp::Diff1 => !x & rest(),
                BitOp::AndOr => x & rest(),
                BitOp::One => {
                    let (mut one, mut many) = (0u8, 0u8);
                    for s in &srcs {
                        let v = at(s, i);
                        many |= one & v;
                        one = (one ^ v) & !many;
                    }
                    one
                }
            };
        }
        out
    };
    let dst = &args[2];
    let n = res.len();
    if n > 0 {
        cx.set_encoded(dst, Value::Str(Str::from_vec(res)), false, true, crate::object::ENC_RAW);
        cx.notify(notify::STRING, "set", dst);
    } else if cx.delete(dst) {
        cx.notify(notify::GENERIC, "del", dst);
    }
    Ok(Reply::Int(n as i64))
}

// ---- BITFIELD ----

#[derive(Clone, Copy, PartialEq)]
enum Overflow {
    Wrap,
    Sat,
    Fail,
}

#[derive(Clone, Copy, PartialEq)]
enum FieldOp {
    Get,
    Set,
    Incr,
}

struct Field {
    op: FieldOp,
    offset: u64,
    value: i64,
    ow: Overflow,
    bits: u32,
    signed: bool,
}

/// `i1`..`i64` / `u1`..`u63`.
fn field_type(a: &[u8]) -> Result<(bool, u32), Error> {
    let signed = match a.first() {
        Some(b'i') => true,
        Some(b'u') => false,
        _ => return Err(TYPE_ERR),
    };
    // The width is read as a C string.
    let rest = &a[1..];
    let rest = &rest[..rest.iter().position(|&c| c == 0).unwrap_or(rest.len())];
    match parse_i64(rest) {
        Some(n) if n >= 1 && ((signed && n <= 64) || (!signed && n <= 63)) => Ok((signed, n as u32)),
        _ => Err(TYPE_ERR),
    }
}

fn get_unsigned(p: &[u8], mut offset: u64, bits: u32) -> u64 {
    let mut v = 0u64;
    for _ in 0..bits {
        let byte = (offset >> 3) as usize;
        let bit = 7 - (offset & 7) as u32;
        let bv = p.get(byte).map_or(0, |b| (b >> bit) & 1);
        v = (v << 1) | bv as u64;
        offset += 1;
    }
    v
}

fn get_signed(p: &[u8], offset: u64, bits: u32) -> i64 {
    let mut v = get_unsigned(p, offset, bits) as i64;
    if bits < 64 && v & (1i64 << (bits - 1)) != 0 {
        v |= -1i64 << bits;
    }
    v
}

fn set_unsigned(p: &mut [u8], mut offset: u64, bits: u32, value: u64) {
    for j in 0..bits {
        let bitval = ((value >> (bits - 1 - j)) & 1) as u8;
        let byte = (offset >> 3) as usize;
        let bit = 7 - (offset & 7) as u32;
        p[byte] = (p[byte] & !(1 << bit)) | (bitval << bit);
        offset += 1;
    }
}

/// `checkUnsignedBitfieldOverflow`: 1 / -1 on overflow / underflow (with the wrapped or
/// saturated value in `limit`), 0 otherwise.
fn unsigned_overflow(value: u64, incr: i64, bits: u32, ow: Overflow, limit: &mut u64) -> i32 {
    let max: u64 = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
    let maxincr = max.wrapping_sub(value) as i64;
    let minincr = value.wrapping_neg() as i64;
    let wrap = |limit: &mut u64| {
        let mask = if bits >= 64 { 0 } else { u64::MAX << bits };
        *limit = value.wrapping_add(incr as u64) & !mask;
    };
    if value > max || (incr > 0 && incr > maxincr) {
        match ow {
            Overflow::Wrap => wrap(limit),
            Overflow::Sat => *limit = max,
            Overflow::Fail => {}
        }
        return 1;
    } else if incr < 0 && incr < minincr {
        match ow {
            Overflow::Wrap => {
                wrap(limit);
                return 1;
            }
            Overflow::Sat => *limit = 0,
            Overflow::Fail => {}
        }
        return -1;
    }
    0
}

/// `checkSignedBitfieldOverflow`.
fn signed_overflow(value: i64, incr: i64, bits: u32, ow: Overflow, limit: &mut i64) -> i32 {
    let max: i64 = if bits == 64 { i64::MAX } else { (1i64 << (bits - 1)) - 1 };
    let min = -max - 1;
    let maxincr = (max as u64).wrapping_sub(value as u64) as i64;
    let minincr = min.wrapping_sub(value);
    let wrap = |limit: &mut i64| {
        let msb = 1u64 << (bits - 1);
        let mut c = (value as u64).wrapping_add(incr as u64);
        if bits < 64 {
            let mask = u64::MAX << bits;
            if c & msb != 0 {
                c |= mask;
            } else {
                c &= !mask;
            }
        }
        *limit = c as i64;
    };
    if value > max || (bits != 64 && incr > maxincr) || (value >= 0 && incr > 0 && incr > maxincr) {
        match ow {
            Overflow::Wrap => wrap(limit),
            Overflow::Sat => *limit = max,
            Overflow::Fail => {}
        }
        return 1;
    } else if value < min || (bits != 64 && incr < minincr) || (value < 0 && incr < 0 && incr < minincr) {
        match ow {
            Overflow::Wrap => {
                wrap(limit);
                return 1;
            }
            Overflow::Sat => *limit = min,
            Overflow::Fail => {}
        }
        return -1;
    }
    0
}

fn read_field(p: &[u8], f: &Field) -> i64 {
    if f.signed { get_signed(p, f.offset, f.bits) } else { get_unsigned(p, f.offset, f.bits) as i64 }
}

fn bitfield_generic(cx: &mut Cx, args: &[Bytes], read_only_cmd: bool) -> CmdResult {
    let mut fields = Vec::new();
    let mut ow = Overflow::Wrap;
    let mut readonly = true;
    let mut highest: u64 = 0;
    let mut j = 2;
    while j < args.len() {
        let rem = args.len() - j - 1;
        let sub = &args[j];
        let op = if eq(sub, "get") && rem >= 2 {
            FieldOp::Get
        } else if eq(sub, "set") && rem >= 3 {
            FieldOp::Set
        } else if eq(sub, "incrby") && rem >= 3 {
            FieldOp::Incr
        } else if eq(sub, "overflow") && rem >= 1 {
            let t = &args[j + 1];
            ow = if eq(t, "wrap") {
                Overflow::Wrap
            } else if eq(t, "sat") {
                Overflow::Sat
            } else if eq(t, "fail") {
                Overflow::Fail
            } else {
                return Err(Error::new("ERR Invalid OVERFLOW type specified"));
            };
            j += 2;
            continue;
        } else {
            return Err(SYNTAX);
        };
        let (signed, bits) = field_type(&args[j + 1])?;
        let offset = bit_offset(cx, &args[j + 2], true, bits)?;
        let mut value = 0;
        if op != FieldOp::Get {
            readonly = false;
            highest = highest.max(offset + bits as u64 - 1);
            value = int(&args[j + 3])?;
        }
        fields.push(Field { op, offset, value, ow, bits, signed });
        j += if op == FieldOp::Get { 3 } else { 4 };
    }

    if readonly {
        let s: &[u8] = match cx.get(&args[1]) {
            None => &[],
            Some(Value::Str(s)) => s,
            Some(_) => return Err(WRONGTYPE),
        };
        return Ok(Reply::Array(fields.iter().map(|f| Reply::Int(read_field(s, f))).collect()));
    }
    if read_only_cmd {
        return Err(Error::new("ERR BITFIELD_RO only supports the GET subcommand"));
    }

    let need = (highest >> 3) as usize + 1;
    let old_len = str_len(cx, &args[1]);
    let mut e = cx.get_or_create(&args[1], Kind::String)?;
    let s = if old_len.is_some() { e.str_in_place(need) } else { e.str_new_raw() };
    let grown = grow_zero(s, need);
    let new_len = s.len();
    let mut changes = 0;
    let mut out = Vec::with_capacity(fields.len());
    for f in &fields {
        if f.op == FieldOp::Get {
            out.push(Reply::Int(read_field(s, f)));
            continue;
        }
        let (overflow, ret, newval): (i32, i64, u64) = if f.signed {
            let old = get_signed(s, f.offset, f.bits);
            let mut wrapped = 0i64;
            if f.op == FieldOp::Incr {
                let o = signed_overflow(old, f.value, f.bits, f.ow, &mut wrapped);
                let new = if o != 0 { wrapped } else { old.wrapping_add(f.value) };
                (o, new, new as u64)
            } else {
                let o = signed_overflow(f.value, 0, f.bits, f.ow, &mut wrapped);
                (o, old, if o != 0 { wrapped as u64 } else { f.value as u64 })
            }
        } else {
            let old = get_unsigned(s, f.offset, f.bits);
            let mut wrapped = 0u64;
            if f.op == FieldOp::Incr {
                let o = unsigned_overflow(old, f.value, f.bits, f.ow, &mut wrapped);
                let new = if o != 0 { wrapped } else { old.wrapping_add(f.value as u64) };
                (o, new as i64, new)
            } else {
                let o = unsigned_overflow(f.value as u64, 0, f.bits, f.ow, &mut wrapped);
                (o, old as i64, if o != 0 { wrapped } else { f.value as u64 })
            }
        };
        if overflow != 0 && f.ow == Overflow::Fail {
            out.push(Reply::Nil);
        } else {
            out.push(Reply::Int(ret));
            let before = get_unsigned(s, f.offset, f.bits);
            set_unsigned(s, f.offset, f.bits, newval);
            // A write counts (notification, propagation) if the key was created or grown, or
            // the field's bits changed.
            if grown || before != get_unsigned(s, f.offset, f.bits) {
                changes += 1;
            }
        }
    }
    if changes > 0 {
        e.notify(notify::STRING, "setbit");
    } else if !grown {
        e.unchanged();
    }
    e.changes(changes);
    drop(e);
    // Redis moves an existing string's keysizes bin only when it had bytes and something
    // changed (`changes && strOldSize > 0`), though the string grew either way.
    if let Some(old_len) = old_len
        && grown
        && (changes == 0 || old_len == 0)
    {
        let db = cx.db_index();
        cx.eng.dbs[db].keysizes_skip_move(old_len, new_len);
    }
    Ok(Reply::Array(out))
}

/// The length of the string at `key` (`None` if missing; other types are refused after).
fn str_len(cx: &mut Cx, key: &[u8]) -> Option<usize> {
    cx.peek(key).map(|e| match &e.value {
        Value::Str(s) => s.len(),
        _ => 0,
    })
}

/// BITFIELD key [GET type offset | [OVERFLOW WRAP|SAT|FAIL] SET type offset value |
///     [OVERFLOW WRAP|SAT|FAIL] INCRBY type offset increment] …
pub fn bitfield(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    bitfield_generic(cx, args, false)
}

pub fn bitfield_ro(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    bitfield_generic(cx, args, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Client, Engine};

    fn call(e: &mut Engine, c: &mut Client, args: &[&[u8]]) -> Reply {
        let a: Vec<Bytes> = args.iter().map(|s| Bytes::copy_from_slice(s)).collect();
        e.call(c, &a).unwrap_reply()
    }

    #[test]
    fn bits() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        assert_eq!(call(&mut e, &mut c, &[b"SETBIT", b"k", b"7", b"1"]), Reply::Int(0));
        assert_eq!(call(&mut e, &mut c, &[b"GET", b"k"]), Reply::bulk([1u8]));
        assert_eq!(call(&mut e, &mut c, &[b"SET", b"s", b"foobar"]), Reply::Ok);
        assert_eq!(call(&mut e, &mut c, &[b"BITCOUNT", b"s"]), Reply::Int(26));
        assert_eq!(call(&mut e, &mut c, &[b"BITCOUNT", b"s", b"5", b"30", b"BIT"]), Reply::Int(17));
        assert_eq!(call(&mut e, &mut c, &[b"SET", b"z", b"\xff\xf0\x00"]), Reply::Ok);
        assert_eq!(call(&mut e, &mut c, &[b"BITPOS", b"z", b"0"]), Reply::Int(12));
        assert_eq!(call(&mut e, &mut c, &[b"BITPOS", b"z", b"1", b"7", b"15", b"bit"]), Reply::Int(7));
        assert_eq!(call(&mut e, &mut c, &[b"BITPOS", b"z", b"0", b"7", b"15", b"bit"]), Reply::Int(12));
    }

    #[test]
    fn bitops() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        call(&mut e, &mut c, &[b"SET", b"a", b"\xff\x0f"]);
        call(&mut e, &mut c, &[b"SET", b"b", b"\xf0"]);
        call(&mut e, &mut c, &[b"SET", b"c", b"\x01\x02\x03"]);
        let get = |e: &mut Engine, c: &mut Client| call(e, c, &[b"GET", b"d"]);
        for (op, want) in [
            (&b"DIFF"[..], &b"\x0e\r\x00"[..]),
            (b"DIFF1", b"\x00\x00\x03"),
            (b"ANDOR", b"\xf1\x02\x00"),
            (b"ONE", b"\x0e\r\x03"),
            (b"XOR", b"\x0e\r\x03"),
        ] {
            assert_eq!(call(&mut e, &mut c, &[b"BITOP", op, b"d", b"a", b"b", b"c"]), Reply::Int(3));
            assert_eq!(get(&mut e, &mut c), Reply::bulk(want));
        }
    }

    #[test]
    fn bitfield_overflow() {
        let mut e = Engine::standalone();
        let mut c = Client::new(1);
        let r = call(&mut e, &mut c, &[b"BITFIELD", b"k", b"SET", b"u8", b"0", b"255", b"GET", b"u8", b"0"]);
        assert_eq!(r, Reply::Array(vec![Reply::Int(0), Reply::Int(255)]));
        let r = call(&mut e, &mut c, &[b"BITFIELD", b"k", b"INCRBY", b"u8", b"0", b"10"]);
        assert_eq!(r, Reply::Array(vec![Reply::Int(9)]));
        let r = call(&mut e, &mut c, &[b"BITFIELD", b"k", b"OVERFLOW", b"FAIL", b"INCRBY", b"u8", b"0", b"300"]);
        assert_eq!(r, Reply::Array(vec![Reply::Nil]));
        let r = call(&mut e, &mut c, &[b"BITFIELD", b"k", b"OVERFLOW", b"SAT", b"INCRBY", b"i8", b"0", b"-300"]);
        assert_eq!(r, Reply::Array(vec![Reply::Int(-128)]));
    }
}
