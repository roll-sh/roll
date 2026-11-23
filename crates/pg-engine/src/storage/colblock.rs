//! Storage format 5 (agent 282's design, HANDOFF "Storage format 5: column blocks"): a bottom
//! level block of a table's rows may store its values as columns instead of the rows' bytes.
//! Only a store whose data directory has them switched on writes them (`StoreOptions::
//! colblocks`, off by default); every build since reads them.
//!
//! A format 2 block is its entries (keys and value lengths), then its values (the rows' bytes,
//! in entry order), then its restarts and trailer. A column block keeps the entries, restarts
//! and trailer as they are and replaces the values with a column section: per column of the
//! block's rows, its values in one of a few layouts (`Kind`), each value at a place computed
//! from its row number. The section is lossless: `ColDir::row` makes each row's bytes back
//! exactly (the writer checks this before it writes a block so), so every reader that wants
//! rows gets the same bytes as from a format 4 block, and value offsets in the entries keep
//! meaning what they meant (offsets into the rows' bytes, `vbase` where they start).
//!
//! A block becomes one only when every entry is a row (a stored value, not a deletion or a
//! derived index value) with the same header kind (`ROW_HEADER` or `ROW_HEADER_TID`, none with
//! values stored out of line) and the same column count; any other block stays format 2.
//!
//! Section layout (all offsets in a parsed `ColDir` are into the block's buffer):
//! `MAGIC, varint n, varint ncols, varint vlen, tid byte, then the header columns (xmin, and with
//! TIDs the block and offset: each `Ints`: width byte, varint base), then per column its kind byte
//! and parameters and a has-NULLs byte; then the payloads in that order; 8 zero bytes; u32 vbase`.

use super::row::{ROW_HEADER, ROW_HEADER_TID, tag};
use super::sst::{get_varint, put_varint};

pub const MAGIC: u8 = 0xC5;
const NONE: u32 = u32::MAX;

/// How a column's values are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Every row NULL.
    Null,
    /// A value of tag `tag` and `w` bytes after it in every row not NULL: `n * w` bytes.
    Fixed { tag: u8, w: u8 },
    /// Booleans: a byte per row (the tag).
    Bool,
    /// Numerics of one scale, the unscaled value in `w` bytes (two's complement, sign-extended
    /// on read); each row stores the shortest form (`i128_signed_le`).
    Num { scale: u32, w: u8 },
    /// Strings of tag `tag`: `n + 1` offsets of `ow` bytes (into the bytes after them), the bytes.
    Str { tag: u8, ow: u8 },
    /// Strings of tag `tag` out of `k` distinct ones: a code byte per row, `k + 1` u16 offsets,
    /// the distinct strings' bytes.
    Dict { tag: u8, k: u16 },
    /// Each row's value as the row stores it (tag and payload): `n + 1` offsets of `ow` bytes,
    /// the bytes.
    Raw { ow: u8 },
}

/// A column's layout and where its parts are.
#[derive(Clone, Copy, Debug)]
pub struct Col {
    pub kind: Kind,
    /// The NULL bitmap (bit r: row r), or `NONE`.
    pub nulls: u32,
    /// The values (`Fixed`, `Bool`, `Num`), the offsets (`Str`, `Raw`), the codes (`Dict`).
    pub at: u32,
    /// `Str`, `Raw`: the bytes after the offsets; `Dict`: the dictionary's offsets.
    pub aux: u32,
    /// `Dict`: the dictionary's bytes.
    pub aux2: u32,
}

/// Header numbers: `base` plus a `w`-byte number per row (`w` 0: `base` in every row).
#[derive(Clone, Copy, Debug, Default)]
struct Ints {
    w: u8,
    base: u64,
    at: u32,
}

/// A parsed column section.
#[derive(Debug)]
pub struct ColDir {
    pub n: u32,
    pub ncols: u32,
    pub tid: bool,
    xmin: Ints,
    tb: Ints,
    to: Ints,
    pub cols: Vec<Col>,
    /// The rows' bytes, all of them (what the values took in format 2).
    pub vlen: u32,
    /// Where the rows' bytes would start (the entries' end).
    pub vbase: u32,
}

#[inline(always)]
fn word(d: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(d[at..at + 8].try_into().unwrap())
}

/// The `w`-byte (1-8) little-endian number at `at`, read as a word (the section ends in 8 zero
/// bytes, so the word is always there).
#[inline(always)]
pub fn uint(d: &[u8], at: usize, w: u8) -> u64 {
    if w == 0 {
        return 0;
    }
    word(d, at) & (u64::MAX >> (64 - 8 * w as u32))
}

/// `uint` sign-extended from its `w` bytes.
#[inline(always)]
pub fn sint(d: &[u8], at: usize, w: u8) -> i64 {
    let sh = 64 - 8 * w as u32;
    ((word(d, at) << sh) as i64) >> sh
}

#[inline(always)]
pub fn is_null(d: &[u8], nulls: u32, r: usize) -> bool {
    nulls != NONE && d[nulls as usize + r / 8] >> (r % 8) & 1 != 0
}

fn width_of(max: u64) -> u8 {
    match max {
        0 => 0,
        1..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFFFF_FFFF => 4,
        _ => 8,
    }
}

fn varint_len(v: u64) -> usize {
    (64 - (v | 1).leading_zeros()).div_ceil(7) as usize
}

impl Ints {
    fn get(&self, d: &[u8], r: usize) -> u64 {
        self.base.wrapping_add(uint(d, self.at as usize + r * self.w as usize, self.w))
    }
}

/// The rows' header numbers and their values' spans (within each row), as the writer splits them.
struct Split<'a> {
    rows: &'a [&'a [u8]],
    tid: bool,
    hdr: Vec<[u64; 3]>,
    /// Per row, per column: (start, end) in the row.
    spans: Vec<(u32, u32)>,
    ncols: usize,
}

fn split<'a>(rows: &'a [&'a [u8]]) -> Option<Split<'a>> {
    let first = *rows.first()?.first()?;
    let tid = match first {
        ROW_HEADER => false,
        ROW_HEADER_TID => true,
        _ => return None,
    };
    let mut hdr = Vec::with_capacity(rows.len());
    let mut spans = Vec::new();
    let mut ncols = None;
    for v in rows {
        if v.first() != Some(&first) {
            return None;
        }
        let mut p = 1;
        let mut h = [0u64; 3];
        h[0] = get_varint(v, &mut p).ok()?;
        if tid {
            h[1] = get_varint(v, &mut p).ok()?;
            h[2] = get_varint(v, &mut p).ok()?;
        }
        hdr.push(h);
        let c = get_varint(v, &mut p).ok()? as usize;
        if *ncols.get_or_insert(c) != c {
            return None;
        }
        for _ in 0..c {
            let s = p;
            super::row::skip_value(v, &mut p).ok()?;
            spans.push((s as u32, p as u32));
        }
        if p != v.len() {
            return None;
        }
    }
    Some(Split { rows, tid, hdr, spans, ncols: ncols.unwrap_or(0) })
}

fn put_ints(dir: &mut Vec<u8>, pay: &mut Vec<u8>, vals: impl Iterator<Item = u64> + Clone) {
    let base = vals.clone().min().unwrap_or(0);
    let w = width_of(vals.clone().map(|v| v - base).max().unwrap_or(0));
    dir.push(w);
    put_varint(dir, base);
    if w > 0 {
        for v in vals {
            pay.extend_from_slice(&(v - base).to_le_bytes()[..w as usize]);
        }
    }
}

fn fixed_width(t: u8) -> Option<u8> {
    Some(match t {
        tag::INT2 => 2,
        tag::INT4 | tag::DATE | tag::FLOAT4 | tag::OID => 4,
        tag::INT8 | tag::FLOAT8 | tag::TIMESTAMP | tag::TIMESTAMPTZ | tag::TIME | tag::MONEY | tag::LSN | tag::XID8 => 8,
        _ => return None,
    })
}

/// A string value's bytes when it is `tag`, its length as `put_varint` writes it, then them.
fn str_body(v: &[u8]) -> Option<&[u8]> {
    let mut p = 1;
    let n = get_varint(v, &mut p).ok()? as usize;
    (p == 1 + varint_len(n as u64) && p + n == v.len()).then(|| &v[p..])
}

/// A numeric value's (scale, unscaled) when it is stored shortest in 1-8 bytes.
fn num_parts(v: &[u8]) -> Option<(u32, i64, usize)> {
    let mut p = 1;
    let scale = get_varint(v, &mut p).ok()?;
    if p != 1 + varint_len(scale) || scale > u32::MAX as u64 {
        return None;
    }
    let n = *v.get(p)? as usize;
    p += 1;
    if !(1..=8).contains(&n) || p + n != v.len() {
        return None;
    }
    let mut w = [if v[p + n - 1] & 0x80 != 0 { 0xFF } else { 0 }; 8];
    w[..n].copy_from_slice(&v[p..p + n]);
    let x = i64::from_le_bytes(w);
    // (The shortest form, as the row encoder writes it: else the row wouldn't come back.)
    (crate::types::decint::i128_signed_le(x as i128).1 == n).then_some((scale as u32, x, n))
}

/// `encode` of rows given as their bytes one after another (`vals`) and their lengths.
pub fn encode_lens(vals: &[u8], lens: &[u32], vbase: usize, out: &mut Vec<u8>) -> bool {
    let mut rows: Vec<&[u8]> = Vec::with_capacity(lens.len());
    let mut o = 0usize;
    for l in lens {
        rows.push(&vals[o..o + *l as usize]);
        o += *l as usize;
    }
    encode(&rows, vbase, out)
}

/// The rows (`rows`, each a stored row's bytes) as a column section appended to `out` (the block's
/// buffer, whose entries end at `vbase`). False, `out` as it was: the rows don't fit a column block.
pub fn encode(rows: &[&[u8]], vbase: usize, out: &mut Vec<u8>) -> bool {
    let start = out.len();
    let ok = encode_in(rows, vbase, out);
    if !ok {
        out.truncate(start);
    }
    ok
}

fn encode_in(rows: &[&[u8]], vbase: usize, out: &mut Vec<u8>) -> bool {
    let Some(s) = split(rows) else { return false };
    let n = rows.len();
    let vlen: usize = rows.iter().map(|r| r.len()).sum();
    if vlen > u32::MAX as usize / 2 {
        return false;
    }
    let mut dir = vec![MAGIC];
    put_varint(&mut dir, n as u64);
    put_varint(&mut dir, s.ncols as u64);
    put_varint(&mut dir, vlen as u64);
    dir.push(s.tid as u8);
    let mut pay = Vec::with_capacity(vlen);
    for h in 0..if s.tid { 3 } else { 1 } {
        put_ints(&mut dir, &mut pay, s.hdr.iter().map(move |x| x[h]));
    }
    let val = |r: usize, c: usize| {
        let (a, b) = s.spans[r * s.ncols + c];
        &s.rows[r][a as usize..b as usize]
    };
    for c in 0..s.ncols {
        let nulls: Vec<bool> = (0..n).map(|r| val(r, c)[0] == tag::NULL).collect();
        let any_null = nulls.iter().any(|x| *x);
        let tags: Vec<u8> = (0..n).map(|r| val(r, c)[0]).filter(|t| *t != tag::NULL).collect();
        let put_nulls = |pay: &mut Vec<u8>| {
            if any_null {
                let mut bm = vec![0u8; n.div_ceil(8)];
                for (r, x) in nulls.iter().enumerate() {
                    bm[r / 8] |= (*x as u8) << (r % 8);
                }
                pay.extend_from_slice(&bm);
            }
        };
        let raw = |dir: &mut Vec<u8>, pay: &mut Vec<u8>| {
            let total: usize = (0..n).map(|r| val(r, c).len()).sum();
            let ow: u8 = if total <= 0xFFFF { 2 } else { 4 };
            dir.extend_from_slice(&[6, ow, 0]);
            let mut o = 0usize;
            for r in 0..=n {
                pay.extend_from_slice(&(o as u32).to_le_bytes()[..ow as usize]);
                if r < n {
                    o += val(r, c).len();
                }
            }
            for r in 0..n {
                pay.extend_from_slice(val(r, c));
            }
        };
        let Some(&t) = tags.first() else {
            dir.extend_from_slice(&[0, 0]);
            continue;
        };
        let same = tags.iter().all(|x| *x == t);
        let bools = tags.iter().all(|x| matches!(*x, tag::TRUE | tag::FALSE));
        if bools {
            dir.extend_from_slice(&[2, any_null as u8]);
            put_nulls(&mut pay);
            pay.extend((0..n).map(|r| val(r, c)[0]));
            continue;
        }
        if !same {
            raw(&mut dir, &mut pay);
            continue;
        }
        if let Some(w) = fixed_width(t) {
            dir.extend_from_slice(&[1, t, w, any_null as u8]);
            put_nulls(&mut pay);
            for r in 0..n {
                match nulls[r] {
                    true => pay.extend(std::iter::repeat_n(0, w as usize)),
                    false => pay.extend_from_slice(&val(r, c)[1..]),
                }
            }
            continue;
        }
        if t == tag::NUMERIC {
            let parts: Option<Vec<(u32, i64, usize)>> = (0..n).filter(|r| !nulls[*r]).map(|r| num_parts(val(r, c))).collect();
            if let Some(parts) = parts
                && parts.iter().all(|p| p.0 == parts[0].0)
            {
                let w = parts.iter().map(|p| p.2).max().unwrap_or(1) as u8;
                dir.push(3);
                put_varint(&mut dir, parts[0].0 as u64);
                dir.extend_from_slice(&[w, any_null as u8]);
                put_nulls(&mut pay);
                let mut it = parts.iter();
                for r in 0..n {
                    let x = if nulls[r] { 0 } else { it.next().unwrap().1 };
                    pay.extend_from_slice(&x.to_le_bytes()[..w as usize]);
                }
                continue;
            }
            raw(&mut dir, &mut pay);
            continue;
        }
        if matches!(t, tag::TEXT | tag::BYTEA) {
            let bodies: Option<Vec<&[u8]>> = (0..n).map(|r| if nulls[r] { Some(&[][..]) } else { str_body(val(r, c)) }).collect();
            let Some(bodies) = bodies else {
                raw(&mut dir, &mut pay);
                continue;
            };
            // (Few distinct strings: a dictionary.)
            let mut distinct: Vec<&[u8]> = Vec::new();
            let mut codes = Vec::with_capacity(n);
            for (r, b) in bodies.iter().enumerate() {
                if nulls[r] {
                    codes.push(0u8);
                    continue;
                }
                let k = match distinct.iter().position(|d| d == b) {
                    Some(k) => k,
                    None if distinct.len() < 32 => {
                        distinct.push(b);
                        distinct.len() - 1
                    }
                    None => {
                        distinct.clear();
                        break;
                    }
                };
                codes.push(k as u8);
            }
            let dbytes: usize = distinct.iter().map(|d| d.len()).sum();
            if !distinct.is_empty() && codes.len() == n && dbytes <= 0xFFFF {
                dir.push(5);
                dir.push(t);
                dir.extend_from_slice(&(distinct.len() as u16).to_le_bytes());
                dir.push(any_null as u8);
                put_nulls(&mut pay);
                pay.extend_from_slice(&codes);
                let mut o = 0u16;
                for d in distinct.iter() {
                    pay.extend_from_slice(&o.to_le_bytes());
                    o += d.len() as u16;
                }
                pay.extend_from_slice(&o.to_le_bytes());
                for d in &distinct {
                    pay.extend_from_slice(d);
                }
                continue;
            }
            let total: usize = bodies.iter().map(|b| b.len()).sum();
            let ow: u8 = if total <= 0xFFFF { 2 } else { 4 };
            dir.extend_from_slice(&[4, t, ow, any_null as u8]);
            put_nulls(&mut pay);
            let mut o = 0usize;
            for r in 0..=n {
                pay.extend_from_slice(&(o as u32).to_le_bytes()[..ow as usize]);
                if r < n {
                    o += bodies[r].len();
                }
            }
            for b in &bodies {
                pay.extend_from_slice(b);
            }
            continue;
        }
        raw(&mut dir, &mut pay);
    }
    out.extend_from_slice(&dir);
    out.extend_from_slice(&pay);
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(&(vbase as u32).to_le_bytes());
    // (Checked: every row made back from the section is the row it was.)
    let Some(d) = ColDir::parse(out, vbase, out.len()) else { return false };
    let mut buf = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        buf.clear();
        d.row(out, r, &mut buf);
        if buf != *row {
            return false;
        }
    }
    true
}

impl ColDir {
    /// The heap bytes it holds beside the block's buffer (its box and columns), at the
    /// allocator's sizes.
    pub fn mem_bytes(&self) -> usize {
        use crate::storage::cache::alloc_bytes;
        alloc_bytes(std::mem::size_of::<crate::storage::sst::ColBlk>()) + alloc_bytes(self.cols.capacity() * std::mem::size_of::<Col>())
    }

    /// The section in `d[at..end]` (its last 4 bytes `vbase`).
    pub fn parse(d: &[u8], at: usize, end: usize) -> Option<ColDir> {
        let s = d.get(..end)?;
        if s.get(at) != Some(&MAGIC) || end < at + 13 {
            return None;
        }
        let vbase = u32::from_le_bytes(s[end - 4..end].try_into().unwrap());
        let mut p = at + 1;
        let n = get_varint(s, &mut p).ok()? as usize;
        let ncols = get_varint(s, &mut p).ok()? as usize;
        let vlen = get_varint(s, &mut p).ok()? as u32;
        let tid = *s.get(p)? != 0;
        p += 1;
        let mut ints = [Ints::default(); 3];
        for x in ints.iter_mut().take(if tid { 3 } else { 1 }) {
            x.w = *s.get(p)?;
            p += 1;
            x.base = get_varint(s, &mut p).ok()?;
        }
        // (The directory first, the payloads after it in its order.)
        let mut kinds = Vec::with_capacity(ncols);
        fn b(s: &[u8], p: &mut usize) -> Option<u8> {
            let x = *s.get(*p)?;
            *p += 1;
            Some(x)
        }
        for _ in 0..ncols {
            let k = b(s, &mut p)?;
            let kind = match k {
                0 => Kind::Null,
                1 => {
                    let t = b(s, &mut p)?;
                    Kind::Fixed { tag: t, w: b(s, &mut p)? }
                }
                2 => Kind::Bool,
                3 => {
                    let scale = get_varint(s, &mut p).ok()? as u32;
                    Kind::Num { scale, w: b(s, &mut p)? }
                }
                4 => {
                    let t = b(s, &mut p)?;
                    Kind::Str { tag: t, ow: b(s, &mut p)? }
                }
                5 => {
                    let t = b(s, &mut p)?;
                    let lo = b(s, &mut p)?;
                    Kind::Dict { tag: t, k: u16::from_le_bytes([lo, b(s, &mut p)?]) }
                }
                6 => Kind::Raw { ow: b(s, &mut p)? },
                _ => return None,
            };
            let nulls = b(s, &mut p)?;
            kinds.push((kind, nulls != 0));
        }
        for x in ints.iter_mut().take(if tid { 3 } else { 1 }) {
            x.at = p as u32;
            p += n * x.w as usize;
        }
        let mut cols = Vec::with_capacity(ncols);
        let off = |s: &[u8], at: usize, ow: u8| uint(s, at, ow) as usize;
        for (kind, has_nulls) in kinds {
            let nulls = if has_nulls {
                let x = p as u32;
                p += n.div_ceil(8);
                x
            } else {
                NONE
            };
            let at = p as u32;
            let (aux, aux2) = match kind {
                Kind::Null => (0, 0),
                Kind::Fixed { w, .. } | Kind::Num { w, .. } => {
                    p += n * w as usize;
                    (0, 0)
                }
                Kind::Bool => {
                    p += n;
                    (0, 0)
                }
                Kind::Str { ow, .. } | Kind::Raw { ow } => {
                    let aux = p + (n + 1) * ow as usize;
                    if aux + 8 > end {
                        return None;
                    }
                    p = aux + off(s, at as usize + n * ow as usize, ow);
                    (aux as u32, 0)
                }
                Kind::Dict { k, .. } => {
                    let aux = p + n;
                    let aux2 = aux + (k as usize + 1) * 2;
                    if aux2 + 8 > end {
                        return None;
                    }
                    p = aux2 + off(s, aux + k as usize * 2, 2);
                    (aux as u32, aux2 as u32)
                }
            };
            if p + 12 > end {
                return None;
            }
            cols.push(Col { kind, nulls, at, aux, aux2 });
        }
        (p + 12 == end).then_some(ColDir { n: n as u32, ncols: ncols as u32, tid, xmin: ints[0], tb: ints[1], to: ints[2], cols, vlen, vbase })
    }

    /// Column `c`'s value in row `r` as the row stores it (tag and payload) onto `out`.
    #[inline]
    pub fn value(&self, d: &[u8], c: usize, r: usize, out: &mut Vec<u8>) {
        let col = &self.cols[c];
        if is_null(d, col.nulls, r) || col.kind == Kind::Null {
            out.push(tag::NULL);
            return;
        }
        let at = col.at as usize;
        match col.kind {
            Kind::Null => unreachable!(),
            Kind::Fixed { tag: t, w } => {
                out.push(t);
                out.extend_from_slice(&d[at + r * w as usize..at + (r + 1) * w as usize]);
            }
            Kind::Bool => out.push(d[at + r]),
            Kind::Num { scale, w } => {
                let x = sint(d, at + r * w as usize, w);
                // (The shortest two's complement form, as `i128_signed_le` finds it.)
                let len = (64 - (x ^ (x >> 63)).leading_zeros() + 8) as usize / 8;
                out.push(tag::NUMERIC);
                match scale {
                    0..0x80 => out.push(scale as u8),
                    _ => put_varint(out, scale as u64),
                }
                out.push(len as u8);
                out.extend_from_slice(&x.to_le_bytes()[..len]);
            }
            Kind::Str { tag: t, ow } => {
                let (a, b) = (uint(d, at + r * ow as usize, ow) as usize, uint(d, at + (r + 1) * ow as usize, ow) as usize);
                out.push(t);
                match b - a {
                    n @ 0..0x80 => out.push(n as u8),
                    n => put_varint(out, n as u64),
                }
                let x = col.aux as usize;
                out.extend_from_slice(&d[x + a..x + b]);
            }
            Kind::Dict { tag: t, .. } => {
                let s = self.dict(d, c, d[at + r]);
                out.push(t);
                match s.len() {
                    n @ 0..0x80 => out.push(n as u8),
                    n => put_varint(out, n as u64),
                }
                out.extend_from_slice(s);
            }
            Kind::Raw { ow } => {
                let (a, b) = (uint(d, at + r * ow as usize, ow) as usize, uint(d, at + (r + 1) * ow as usize, ow) as usize);
                let x = col.aux as usize;
                out.extend_from_slice(&d[x + a..x + b]);
            }
        }
    }

    /// Dictionary entry `k` of column `c` (`Kind::Dict`), its (start, end) in `d`.
    #[inline(always)]
    pub fn dict_span(&self, d: &[u8], c: usize, k: u8) -> (usize, usize) {
        let col = &self.cols[c];
        let o = col.aux as usize + k as usize * 2;
        let (a, b) = (u16::from_le_bytes([d[o], d[o + 1]]) as usize, u16::from_le_bytes([d[o + 2], d[o + 3]]) as usize);
        (col.aux2 as usize + a, col.aux2 as usize + b)
    }

    fn dict<'a>(&self, d: &'a [u8], c: usize, k: u8) -> &'a [u8] {
        let (a, b) = self.dict_span(d, c, k);
        &d[a..b]
    }

    /// `Kind::Str`'s row `r`: its bytes' (start, end) in `d`.
    #[inline(always)]
    pub fn str_span(&self, d: &[u8], c: usize, r: usize) -> (usize, usize) {
        let col = &self.cols[c];
        let (Kind::Str { ow, .. } | Kind::Raw { ow }) = col.kind else { return (0, 0) };
        let at = col.at as usize;
        let (a, b) = (uint(d, at + r * ow as usize, ow) as usize, uint(d, at + (r + 1) * ow as usize, ow) as usize);
        (col.aux as usize + a, col.aux as usize + b)
    }

    /// Row `r`'s bytes as format 2 stores them, onto `out`.
    pub fn row(&self, d: &[u8], r: usize, out: &mut Vec<u8>) {
        // (Room for the row's values, each at most 1 + 2 + 8 bytes but strings: grown by those.)
        out.reserve(24 + 11 * self.cols.len());
        out.push(if self.tid { ROW_HEADER_TID } else { ROW_HEADER });
        put_varint(out, self.xmin.get(d, r));
        if self.tid {
            put_varint(out, self.tb.get(d, r));
            put_varint(out, self.to.get(d, r));
        }
        put_varint(out, self.ncols as u64);
        for c in 0..self.ncols as usize {
            self.value(d, c, r, out);
        }
    }

    /// Every row's bytes, in order (`vlen` of them).
    pub fn rows(&self, d: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.vlen as usize);
        for r in 0..self.n as usize {
            self.row(d, r, &mut out);
        }
        debug_assert_eq!(out.len(), self.vlen as usize);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::row;
    use crate::types::Value;
    use crate::types::numeric::Numeric;

    fn stored(r: &[Value], tid: bool, xmin: u64) -> Vec<u8> {
        let mut b = vec![if tid { ROW_HEADER_TID } else { ROW_HEADER }];
        put_varint(&mut b, xmin);
        if tid {
            put_varint(&mut b, xmin / 3);
            put_varint(&mut b, xmin % 300);
        }
        b.extend(row::encode_row(r));
        b
    }

    /// Rows of every kind of column (NULLs, mixed tags, numerics past a word and of two scales,
    /// strings long and few) come back from their section byte for byte, with and without TIDs.
    #[test]
    fn sections_give_their_rows_back() {
        let num = |s: &str| Value::numeric(Numeric::parse_pg(s).unwrap());
        for tid in [false, true] {
            for n in [1usize, 2, 7, 60, 300] {
                let rows: Vec<Vec<Value>> = (0..n)
                    .map(|i| {
                        let i = i as i64;
                        vec![
                            Value::Int4(i as i32 * 7 - 100),
                            if i % 5 == 0 { Value::Null } else { num(&format!("{}.{:02}", i * 37 - 500, i % 100)) },
                            Value::Text(["A", "N", "R"][(i % 3) as usize].into()),
                            Value::Text("x".repeat((i * 13 % 300) as usize).into()),
                            Value::Date(9000 + i as i32),
                            if i % 4 == 0 { Value::Bool(true) } else if i % 4 == 1 { Value::Null } else { Value::Bool(false) },
                            if i % 3 == 0 { Value::Int8(i) } else { Value::Int4(i as i32) },
                            num(if i % 2 == 0 { "123456789012345678901234567890.5" } else { "1.5" }),
                            num(&format!("{}", i - 3)),
                            Value::Null,
                            Value::Float8(i as f64 / 7.0),
                            Value::Interval(crate::types::Interval { months: i as i32, days: 2, micros: 3 }),
                            if i % 2 == 0 { num("1.50") } else { num("2.5") },
                        ]
                    })
                    .collect();
                let enc: Vec<Vec<u8>> = rows.iter().enumerate().map(|(i, r)| stored(r, tid, 1000 + i as u64 * 7)).collect();
                let refs: Vec<&[u8]> = enc.iter().map(|v| v.as_slice()).collect();
                let mut out = vec![9u8; 5];
                assert!(encode(&refs, 5, &mut out), "n {n} tid {tid}");
                let d = ColDir::parse(&out, 5, out.len()).unwrap();
                let all: Vec<u8> = enc.concat();
                assert_eq!(d.rows(&out), all);
                assert_eq!(d.vlen as usize, all.len());
            }
        }
        // (Rows of two column counts, or a row with values stored out of line: not a section.)
        let a = stored(&[Value::Int4(1)], false, 1);
        let b = stored(&[Value::Int4(1), Value::Int4(2)], false, 1);
        let mut out = Vec::new();
        assert!(!encode(&[&a, &b], 0, &mut out) && out.is_empty());
        let mut c = a.clone();
        c[0] = row::ROW_HEADER_EXT;
        assert!(!encode(&[&c], 0, &mut out) && out.is_empty());
    }
}
