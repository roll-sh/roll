//! Stored rows as columns (`ColBatch`): a batch of a scan's rows decoded column by column,
//! so the executor filters and aggregates them in loops over typed vectors (`exec::vector`)
//! instead of making a `Vec<Value>` per row.
//!
//! The interface is agent 149's (HANDOFF "Executor: batch execution"). This body is the
//! executor's stub until 149's typed decode lands under the same signatures.

use super::batch::RowBatch;
use super::row::{ROW_HEADER, ROW_HEADER_EXT, ROW_HEADER_TID, ROW_HEADER_TID_EXT, tag};
use super::sst::get_varint;
use crate::error::Result;
use crate::types::{DataType, Value};
use bytes::Bytes;

/// A column's values, one slot per row (a NULL's or an odd row's slot holds a default).
#[derive(Debug)]
pub enum ColData {
    /// int2, int4.
    I32(Vec<i32>),
    I64(Vec<i64>),
    /// float4, float8.
    F64(Vec<f64>),
    /// Finite numerics that fit an i128: (unscaled, scale).
    Num(Vec<(i128, u32)>),
    Bool(Vec<bool>),
    Date(Vec<i32>),
    /// text, varchar, bpchar (padded as stored), name, bytea: (buf, start, end) into
    /// `ColBatch::bufs`.
    Bytes(Vec<(u32, u32, u32)>),
    /// Other types.
    Values(Vec<Value>),
}

/// One column of a batch.
#[derive(Debug)]
pub struct Col {
    pub data: ColData,
    pub nulls: Vec<bool>,
    /// Rows whose value the typed vector can't hold (a numeric past i128, another tag, a
    /// column the row was stored without), in row order: their slot holds a default.
    pub odd: Vec<(u32, Value)>,
}

/// A batch of a scan's rows as columns.
#[derive(Debug, Default)]
pub struct ColBatch {
    pub len: usize,
    /// By table column; None: not needed.
    pub cols: Vec<Option<Col>>,
    pub bufs: Vec<Bytes>,
    /// Rows with values stored out of line, in order: their needed columns are NULL here.
    pub ext: Vec<u32>,
    own: Vec<u8>,
}

fn data_for(ty: &DataType) -> ColData {
    match ty {
        DataType::Int2 | DataType::Int4 => ColData::I32(Vec::new()),
        DataType::Int8 => ColData::I64(Vec::new()),
        DataType::Float4 | DataType::Float8 => ColData::F64(Vec::new()),
        DataType::Numeric(_) => ColData::Num(Vec::new()),
        DataType::Bool => ColData::Bool(Vec::new()),
        DataType::Date => ColData::Date(Vec::new()),
        DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_) | DataType::Name | DataType::Bytea => {
            ColData::Bytes(Vec::new())
        }
        _ => ColData::Values(Vec::new()),
    }
}

impl ColData {
    fn kind(&self) -> u8 {
        match self {
            ColData::I32(_) => 0,
            ColData::I64(_) => 1,
            ColData::F64(_) => 2,
            ColData::Num(_) => 3,
            ColData::Bool(_) => 4,
            ColData::Date(_) => 5,
            ColData::Bytes(_) => 6,
            ColData::Values(_) => 7,
        }
    }

    /// `n` slots: those it had kept as they are (every row writes its own, a NULL's and an
    /// odd row's its default: `set_default`), new ones a default.
    fn size(&mut self, n: usize) {
        match self {
            ColData::I32(v) | ColData::Date(v) => v.resize(n, 0),
            ColData::I64(v) => v.resize(n, 0),
            ColData::F64(v) => v.resize(n, 0.0),
            ColData::Num(v) => v.resize(n, (0, 0)),
            ColData::Bool(v) => v.resize(n, false),
            ColData::Bytes(v) => v.resize(n, (0, 0, 0)),
            ColData::Values(v) => v.resize(n, Value::Null),
        }
    }

    /// Slot `r` back to the default (a NULL's, an odd row's).
    #[cold]
    fn set_default(&mut self, r: usize) {
        match self {
            ColData::I32(v) | ColData::Date(v) => v[r] = 0,
            ColData::I64(v) => v[r] = 0,
            ColData::F64(v) => v[r] = 0.0,
            ColData::Num(v) => v[r] = (0, 0),
            ColData::Bool(v) => v[r] = false,
            ColData::Bytes(v) => v[r] = (0, 0, 0),
            ColData::Values(v) => v[r] = Value::Null,
        }
    }
}

impl Col {
    /// Row `i`'s bytes (`ColData::Bytes`; empty otherwise).
    #[inline]
    pub fn bytes<'b>(&self, b: &'b ColBatch, i: usize) -> &'b [u8] {
        match &self.data {
            ColData::Bytes(v) => {
                let (buf, s, e) = v[i];
                &b.bufs[buf as usize][s as usize..e as usize]
            }
            _ => &[],
        }
    }

    /// Row `i`'s value, as the row path decodes it for a column of type `ty`.
    pub fn value(&self, b: &ColBatch, i: usize, ty: &DataType) -> Result<Value> {
        if let Ok(k) = self.odd.binary_search_by_key(&(i as u32), |o| o.0) {
            return Ok(self.odd[k].1.clone());
        }
        if self.nulls[i] {
            return Ok(Value::Null);
        }
        Ok(match &self.data {
            ColData::I32(v) if matches!(ty, DataType::Int2) => Value::Int2(v[i] as i16),
            ColData::I32(v) => Value::Int4(v[i]),
            ColData::I64(v) => Value::Int8(v[i]),
            ColData::F64(v) if matches!(ty, DataType::Float4) => Value::Float4(v[i] as f32),
            ColData::F64(v) => Value::Float8(v[i]),
            ColData::Num(v) => Value::numeric(crate::types::numeric::Numeric::from_parts(
                crate::types::decint::DecInt::from(v[i].0),
                v[i].1,
            )),
            ColData::Bool(v) => Value::Bool(v[i]),
            ColData::Date(v) => Value::Date(v[i]),
            ColData::Bytes(_) if matches!(ty, DataType::Bytea) => Value::bytea(self.bytes(b, i).to_vec()),
            ColData::Bytes(_) => Value::text(
                std::str::from_utf8(self.bytes(b, i)).map_err(|_| crate::error::internal("invalid UTF-8 in stored row"))?,
            ),
            ColData::Values(v) => v[i].clone(),
        })
    }
}

/// `x` without its trailing spaces (bpchar's comparison), eight bytes at a time: the
/// spaces at a word's end counted from its leading zero bits once xor-ed with spaces.
#[inline]
pub fn trim_spaces(x: &[u8]) -> &[u8] {
    const SPACES: u64 = 0x2020_2020_2020_2020;
    let mut n = x.len();
    while n >= 8 {
        let w = u64::from_le_bytes(x[n - 8..n].try_into().unwrap()) ^ SPACES;
        if w != 0 {
            return &x[..n - (w.leading_zeros() / 8) as usize];
        }
        n -= 8;
    }
    while n > 0 && x[n - 1] == b' ' {
        n -= 1;
    }
    &x[..n]
}

/// The stored row's encoded values, after its header.
#[inline]
fn row_values(bytes: &[u8]) -> Option<&[u8]> {
    let n = match *bytes.first()? {
        ROW_HEADER | ROW_HEADER_EXT => 1,
        ROW_HEADER_TID | ROW_HEADER_TID_EXT => 3,
        _ => return None,
    };
    let mut pos = 1;
    for _ in 0..n {
        pos += bytes.get(pos..)?.iter().position(|b| *b < 0x80)? + 1;
    }
    bytes.get(pos..)
}

/// A numeric's unscaled value from its stored bytes at `data[at..at + n]` (two's
/// complement, little-endian, as `to_signed_bytes_le` wrote it; None past an i128's 16).
/// Up to 8 bytes with 8 of the row from there are one word load, sign-extended. (Inlined
/// into the walks: a call returned its `Option<i128>` through memory, 8% of q01.)
#[inline(always)]
fn num_bytes(data: &[u8], at: usize, n: usize) -> Option<i128> {
    if n == 0 {
        return Some(0);
    }
    if n <= 8
        && let Some(w) = data.get(at..at + 8)
    {
        let sh = 64 - 8 * n as u32;
        return Some(((u64::from_le_bytes(w.try_into().unwrap()) << sh) as i64 >> sh) as i128);
    }
    let x = data.get(at..at + n)?;
    if n > 16 {
        return None;
    }
    let fill = if x[n - 1] & 0x80 != 0 { 0xFF } else { 0 };
    let mut w = [fill; 16];
    w[..n].copy_from_slice(x);
    Some(i128::from_le_bytes(w))
}

/// The value at `data[*pos]` (tag `t`, not NULL) into `col`'s slot `r`, `*pos` moved past it
/// (false: it doesn't fit, `*pos` unchanged: odd). A string's bytes stay where they are: in
/// `buf` (the block buffer `data` is in, at `base`), else copied into `own` (buffer 0).
#[inline(always)]
fn put_typed(col: &mut Col, r: usize, own: &mut Vec<u8>, data: &[u8], pos: &mut usize, buf: Option<(u32, usize)>) -> bool {
    let p = *pos;
    let t = data[p];
    macro_rules! fixed {
        ($v:expr, $ty:ty, $n:expr, $conv:expr) => {{
            let Some(x) = data.get(p + 1..p + 1 + $n) else { return false };
            $v[r] = $conv(<$ty>::from_le_bytes(x.try_into().unwrap()));
            *pos = p + 1 + $n;
        }};
    }
    match (&mut col.data, t) {
        (ColData::I32(v), tag::INT4) => fixed!(v, i32, 4, |x| x),
        (ColData::I32(v), tag::INT2) => fixed!(v, i16, 2, |x: i16| x as i32),
        (ColData::Date(v), tag::DATE) => fixed!(v, i32, 4, |x| x),
        (ColData::I64(v), tag::INT8) => fixed!(v, i64, 8, |x| x),
        (ColData::F64(v), tag::FLOAT8) => fixed!(v, f64, 8, |x| x),
        (ColData::F64(v), tag::FLOAT4) => fixed!(v, f32, 4, |x: f32| x as f64),
        (ColData::Bool(v), tag::TRUE | tag::FALSE) => {
            v[r] = t == tag::TRUE;
            *pos = p + 1;
        }
        (ColData::Num(v), tag::NUMERIC) => {
            // (Scale and length in a byte each, the common case; `peek_num` otherwise.)
            match (data.get(p + 1), data.get(p + 2)) {
                (Some(&sc), Some(&n)) if sc < 0x80 && n < 0x80 && p + 3 + n as usize <= data.len() => {
                    let Some(x) = num_bytes(data, p + 3, n as usize) else { return false };
                    v[r] = (x, sc as u32);
                    *pos = p + 3 + n as usize;
                }
                _ => {
                    let Some(Some(x)) = super::row::peek_num(data, p) else { return false };
                    let mut q = p + 1;
                    if get_varint(data, &mut q).is_err() {
                        return false;
                    }
                    let Ok(len) = get_varint(data, &mut q) else { return false };
                    v[r] = x;
                    *pos = q + len as usize;
                }
            }
        }
        (ColData::Bytes(v), tag::TEXT | tag::BYTEA) => {
            let mut q = p + 1;
            let n = match data.get(q) {
                Some(&l) if l < 0x80 => {
                    q += 1;
                    l as u64
                }
                _ => match get_varint(data, &mut q) {
                    Ok(n) => n,
                    Err(_) => return false,
                },
            };
            let e = q + n as usize;
            if e > data.len() {
                return false;
            }
            v[r] = match buf {
                Some((b, base)) => (b, (base + q) as u32, (base + e) as u32),
                None => {
                    let s = own.len() as u32;
                    own.extend_from_slice(&data[q..e]);
                    (0, s, own.len() as u32)
                }
            };
            *pos = e;
        }
        (ColData::Values(v), _) => {
            let mut q = p;
            match super::row::decode_value(data, &mut q) {
                Ok(x) => v[r] = x,
                Err(_) => return false,
            }
            *pos = q;
        }
        _ => return false,
    }
    true
}

/// `row::skip_value_fast` with numerics and short strings stepped over in place too.
#[inline(always)]
pub(crate) fn skip(data: &[u8], pos: &mut usize) -> Result<()> {
    let p = *pos;
    let n = match data.get(p) {
        Some(&tag::INT4 | &tag::DATE | &tag::FLOAT4) => 4,
        Some(&tag::NUMERIC) => match (data.get(p + 1), data.get(p + 2)) {
            (Some(&sc), Some(&n)) if sc < 0x80 && n < 0x80 => 2 + n as usize,
            _ => return super::row::skip_value_fast(data, pos),
        },
        Some(&tag::TEXT) => match data.get(p + 1) {
            Some(&l) if l < 0x80 => 1 + l as usize,
            _ => return super::row::skip_value_fast(data, pos),
        },
        Some(&(tag::INT8 | tag::FLOAT8 | tag::TIMESTAMP | tag::TIMESTAMPTZ)) => 8,
        Some(&tag::NULL) => 0,
        _ => return super::row::skip_value_fast(data, pos),
    };
    if p + 1 + n > data.len() {
        return super::row::skip_value_fast(data, pos);
    }
    *pos = p + 1 + n;
    Ok(())
}

/// A step of a row's walk by the columns' declared types (`walk_plan`, `walk_row`): values of
/// columns not needed stepped over, or a needed column's value written into its slot. A
/// value the step doesn't expect (another tag, a numeric's scale or length past a byte, a
/// string of 128 bytes or more, a string outside a block buffer) stops the walk, and the row
/// goes the general way (`decode_cols`).
enum Step<'a> {
    /// `count` values of tag `tag` and `len` bytes after it (or NULLs) stepped over.
    Fixed { tag: u8, len: u8, count: u16 },
    /// `count` numerics stepped over.
    Nums(u16),
    /// `count` strings (text, bytea) stepped over.
    Strs(u16),
    /// `count` booleans stepped over.
    Bools(u16),
    /// A value of another type stepped over (`skip`).
    Any,
    /// int4 or date (`tag`).
    I32 { tag: u8, v: &'a mut [i32], nulls: &'a mut [bool] },
    I16 { v: &'a mut [i32], nulls: &'a mut [bool] },
    I64 { v: &'a mut [i64], nulls: &'a mut [bool] },
    F64 { v: &'a mut [f64], nulls: &'a mut [bool] },
    F32 { v: &'a mut [f64], nulls: &'a mut [bool] },
    Bool { v: &'a mut [bool], nulls: &'a mut [bool] },
    Num { v: &'a mut [(i128, u32)], nulls: &'a mut [bool] },
    /// A string (`tag` text or bytea) left in its block buffer.
    Bytes { tag: u8, v: &'a mut [(u32, u32, u32)], nulls: &'a mut [bool] },
}

impl Step<'_> {
    /// Row `r`'s slot back to a default and not NULL (the general walk writes it again).
    #[cold]
    fn reset(&mut self, r: usize) {
        match self {
            Step::I32 { v, nulls, .. } | Step::I16 { v, nulls } => (v[r], nulls[r]) = (0, false),
            Step::I64 { v, nulls } => (v[r], nulls[r]) = (0, false),
            Step::F64 { v, nulls } | Step::F32 { v, nulls } => (v[r], nulls[r]) = (0.0, false),
            Step::Bool { v, nulls } => (v[r], nulls[r]) = (false, false),
            Step::Num { v, nulls } => (v[r], nulls[r]) = ((0, 0), false),
            Step::Bytes { v, nulls, .. } => (v[r], nulls[r]) = ((0, 0, 0), false),
            Step::Fixed { .. } | Step::Nums(_) | Step::Strs(_) | Step::Bools(_) | Step::Any => {}
        }
    }
}

/// The typed walk of columns `0..cols.len()` (`decode_cols`' slots made, None: not needed),
/// and whether it takes a string (rows then need their block buffer). None: a needed column
/// of a type the walk doesn't write (`ColData::Values`).
fn walk_plan<'a>(cols: &'a mut [Option<Col>], types: &[DataType]) -> Option<(Vec<Step<'a>>, bool)> {
    let mut out: Vec<Step<'a>> = Vec::with_capacity(cols.len());
    let mut strs = false;
    for (c, slot) in cols.iter_mut().enumerate() {
        let Some(Col { data, nulls, .. }) = slot else {
            let fixed = match types.get(c)? {
                DataType::Int2 => Some((tag::INT2, 2)),
                DataType::Int4 => Some((tag::INT4, 4)),
                DataType::Date => Some((tag::DATE, 4)),
                DataType::Float4 => Some((tag::FLOAT4, 4)),
                DataType::Int8 => Some((tag::INT8, 8)),
                DataType::Float8 => Some((tag::FLOAT8, 8)),
                DataType::Timestamp(_) => Some((tag::TIMESTAMP, 8)),
                DataType::Timestamptz(_) => Some((tag::TIMESTAMPTZ, 8)),
                _ => None,
            };
            let st = match (fixed, types.get(c)?) {
                (Some((tag, len)), _) => Step::Fixed { tag, len, count: 1 },
                (None, DataType::Numeric(_)) => Step::Nums(1),
                (None, DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_) | DataType::Name | DataType::Bytea) => Step::Strs(1),
                (None, DataType::Bool) => Step::Bools(1),
                _ => Step::Any,
            };
            // (Runs of one kind stepped over as one step.)
            match (out.last_mut(), &st) {
                (Some(Step::Fixed { tag, len, count }), Step::Fixed { tag: t, len: l, .. }) if tag == t && len == l && *count < u16::MAX => *count += 1,
                (Some(Step::Nums(n)), Step::Nums(_)) | (Some(Step::Strs(n)), Step::Strs(_)) | (Some(Step::Bools(n)), Step::Bools(_)) if *n < u16::MAX => *n += 1,
                _ => out.push(st),
            }
            continue;
        };
        let (nulls, ty) = (&mut nulls[..], types.get(c)?);
        let (int2, f4, bytea) = (matches!(ty, DataType::Int2), matches!(ty, DataType::Float4), matches!(ty, DataType::Bytea));
        out.push(match data {
            ColData::I32(v) => match int2 {
                true => Step::I16 { v, nulls },
                false => Step::I32 { tag: tag::INT4, v, nulls },
            },
            ColData::Date(v) => Step::I32 { tag: tag::DATE, v, nulls },
            ColData::I64(v) => Step::I64 { v, nulls },
            ColData::F64(v) => match f4 {
                true => Step::F32 { v, nulls },
                false => Step::F64 { v, nulls },
            },
            ColData::Bool(v) => Step::Bool { v, nulls },
            ColData::Num(v) => Step::Num { v, nulls },
            ColData::Bytes(v) => {
                strs = true;
                Step::Bytes { tag: if bytea { tag::BYTEA } else { tag::TEXT }, v, nulls }
            }
            ColData::Values(_) => return None,
        });
    }
    Some((out, strs))
}

/// Row `r` (its stored bytes `bytes`; in block buffer `buf` at `at`, or None) walked by
/// `steps` (`walk_plan`'s for columns `0..upto`); false: the row isn't one the walk takes
/// (stored out of line, without some of the columns, a value it doesn't expect): the slots
/// it wrote are reset by the caller and the general walk does the row.
#[inline]
fn walk_row(steps: &mut [Step<'_>], r: usize, bytes: &[u8], buf: Option<(u32, usize)>, upto: usize) -> bool {
    if !matches!(bytes.first(), Some(&(ROW_HEADER | ROW_HEADER_TID))) {
        return false;
    }
    let Some(data) = row_values(bytes) else { return false };
    let base = bytes.len() - data.len();
    let mut pos = match data.first() {
        Some(&n) if n < 0x80 && n as usize >= upto => 1,
        _ => return false,
    };
    // (A NULL's tag alone; a value of another tag stops the walk.)
    macro_rules! null_or {
        ($v:expr, $nulls:expr, $d:expr) => {{
            if data.get(pos) != Some(&tag::NULL) {
                return false;
            }
            ($v[r], $nulls[r]) = ($d, true);
            pos += 1;
        }};
    }
    macro_rules! fixed {
        ($v:expr, $nulls:expr, $tag:expr, $ty:ty, $n:expr, $conv:expr) => {{
            match data.get(pos..pos + 1 + $n) {
                Some(x) if x[0] == $tag => {
                    $v[r] = $conv(<$ty>::from_le_bytes(x[1..].try_into().unwrap()));
                    pos += 1 + $n;
                }
                _ => null_or!($v, $nulls, Default::default()),
            }
        }};
    }
    for st in steps.iter_mut() {
        match st {
            Step::Fixed { tag, len, count } => {
                let w = 1 + *len as usize;
                for _ in 0..*count {
                    match data.get(pos) {
                        Some(t) if t == tag => pos += w,
                        Some(&tag::NULL) => pos += 1,
                        _ => return false,
                    }
                }
            }
            // (A value's tag and length bytes read with one bounds check; scale and length
            // under 0x80 together: their OR.)
            Step::Nums(count) => {
                for _ in 0..*count {
                    match data.get(pos..pos + 3) {
                        Some(&[tag::NUMERIC, sc, n]) if (sc | n) < 0x80 => pos += 3 + n as usize,
                        _ if data.get(pos) == Some(&tag::NULL) => pos += 1,
                        _ => return false,
                    }
                }
            }
            Step::Strs(count) => {
                for _ in 0..*count {
                    match data.get(pos..pos + 2) {
                        Some(&[tag::TEXT | tag::BYTEA, l]) if l < 0x80 => pos += 2 + l as usize,
                        _ if data.get(pos) == Some(&tag::NULL) => pos += 1,
                        _ => return false,
                    }
                }
            }
            Step::Bools(count) => {
                for _ in 0..*count {
                    match data.get(pos) {
                        Some(&(tag::TRUE | tag::FALSE | tag::NULL)) => pos += 1,
                        _ => return false,
                    }
                }
            }
            // (Through a copy: `pos` itself passed by reference to `skip`'s out-of-line call
            // was kept in memory, stored and loaded again around every step.)
            Step::Any => {
                let mut q = pos;
                if skip(data, &mut q).is_err() {
                    return false;
                }
                pos = q;
            }
            Step::I32 { tag, v, nulls } => fixed!(v, nulls, *tag, i32, 4, |x| x),
            Step::I16 { v, nulls } => fixed!(v, nulls, tag::INT2, i16, 2, |x: i16| x as i32),
            Step::I64 { v, nulls } => fixed!(v, nulls, tag::INT8, i64, 8, |x| x),
            Step::F64 { v, nulls } => fixed!(v, nulls, tag::FLOAT8, f64, 8, |x| x),
            Step::F32 { v, nulls } => fixed!(v, nulls, tag::FLOAT4, f32, 4, |x: f32| x as f64),
            Step::Bool { v, nulls } => match data.get(pos) {
                Some(&t @ (tag::TRUE | tag::FALSE)) => {
                    v[r] = t == tag::TRUE;
                    pos += 1;
                }
                _ => null_or!(v, nulls, Default::default()),
            },
            // (Its tag, scale and length and the 8 bytes after them under one bounds check, which
            // doesn't check `pos + 11` for overflow: up to 8 bytes are one word load,
            // sign-extended. Checked apart, `data.get(pos..pos + 3)`, the value's end and the
            // word's, the PGO build merged the checks into setcc pairs, +4% of q06's walk.)
            Step::Num { v, nulls } => match data.get(pos..).and_then(|x| x.first_chunk::<11>()) {
                Some(&[tag::NUMERIC, sc, n, ref w @ ..]) if (sc | n) < 0x80 && n <= 8 => {
                    v[r] = match n {
                        0 => (0, sc as u32),
                        n => {
                            let sh = 64 - 8 * n as u32;
                            (((u64::from_le_bytes(*w) << sh) as i64 >> sh) as i128, sc as u32)
                        }
                    };
                    pos += 3 + n as usize;
                }
                // (Near the row's end, or longer.)
                _ => match data.get(pos..pos + 3) {
                    Some(&[tag::NUMERIC, sc, n]) if (sc | n) < 0x80 && pos + 3 + n as usize <= data.len() => {
                        let Some(x) = num_bytes(data, pos + 3, n as usize) else { return false };
                        v[r] = (x, sc as u32);
                        pos += 3 + n as usize;
                    }
                    _ => null_or!(v, nulls, Default::default()),
                },
            },
            Step::Bytes { tag, v, nulls } => match (data.get(pos..pos + 2), buf) {
                (Some(&[t, l]), Some((b, at))) if t == *tag && l < 0x80 && pos + 2 + l as usize <= data.len() => {
                    let s = (at + base + pos + 2) as u32;
                    v[r] = (b, s, s + l as u32);
                    pos += 2 + l as usize;
                }
                _ => null_or!(v, nulls, Default::default()),
            },
        }
    }
    // (A skipped value past the row's end: the general walk raises it.)
    pos <= data.len()
}

/// Rows `rows` of `batch` as columns into `out`: the columns `need` marks, decoded as
/// `types` declares them, a column a row was stored without as `missing`'s value.
pub fn decode_cols(
    batch: &RowBatch,
    rows: std::ops::Range<usize>,
    need: &[bool],
    types: &[DataType],
    missing: &[Value],
    out: &mut ColBatch,
) -> Result<()> {
    let ncols = need.len().min(types.len());
    let len = rows.len();
    out.cols.resize_with(ncols.max(out.cols.len()), || None);
    // (Every needed column's slots made at once, a default in each: a row writes its own.)
    for (c, slot) in out.cols.iter_mut().enumerate() {
        if c >= ncols || !need[c] {
            *slot = None;
            continue;
        }
        let want = data_for(&types[c]);
        match slot {
            Some(col) if col.data.kind() == want.kind() => {
                col.nulls.clear();
                col.odd.clear();
            }
            _ => *slot = Some(Col { data: want, nulls: Vec::new(), odd: Vec::new() }),
        }
        // (A slot kept from the batch before is written by its row: not filled first.)
        let col = slot.as_mut().unwrap();
        col.data.size(len);
        col.nulls.resize(len, false);
    }
    out.len = len;
    out.bufs.clear();
    out.ext.clear();
    out.own.clear();
    // (Buffer 0 is `own`, put in place at the end; the block buffers rows are in follow,
    // each once while consecutive rows share it.)
    out.bufs.push(Bytes::new());
    let upto = need.iter().take(ncols).rposition(|b| *b).map_or(0, |c| c + 1);
    // (The batch's parts borrowed apart: the loops keep them in registers.)
    let ColBatch { cols, bufs, ext: exts, own, .. } = out;
    let cols = &mut cols[..upto];
    // (Rows the typed walk didn't take, in order: the general walk's.)
    let mut left: Vec<u32> = Vec::new();
    let all_left = match walk_plan(cols, types) {
        Some((mut steps, strs)) => {
            for (r, i) in rows.clone().enumerate() {
                let (bytes, inbuf) = batch.value_in(i);
                let buf = match (strs, inbuf) {
                    (true, Some((b, at))) => {
                        let last = bufs.last().unwrap();
                        if bufs.len() == 1 || last.as_ptr() != b.as_ptr() || last.len() != b.len() {
                            bufs.push(b.clone());
                        }
                        Some(((bufs.len() - 1) as u32, at))
                    }
                    _ => None,
                };
                if !walk_row(&mut steps, r, bytes, buf, upto) {
                    for st in steps.iter_mut() {
                        st.reset(r);
                    }
                    left.push(r as u32);
                }
            }
            false
        }
        None => true,
    };
    let mut row = |r: usize, i: usize| -> Result<()> {
        let (bytes, inbuf) = batch.value_in(i);
        let ext = matches!(bytes.first(), Some(&(ROW_HEADER_EXT | ROW_HEADER_TID_EXT)));
        if ext {
            exts.push(r as u32);
        }
        let data = row_values(bytes).ok_or_else(|| crate::error::internal("stored row without a header"))?;
        let buf = inbuf.map(|(b, _)| {
            let last = bufs.last().unwrap();
            if bufs.len() == 1 || last.as_ptr() != b.as_ptr() || last.len() != b.len() {
                bufs.push(b.clone());
            }
            ((bufs.len() - 1) as u32, data.as_ptr() as usize - b.as_ptr() as usize)
        });
        let mut pos = 0;
        let n = match data.first() {
            Some(&b) if b < 0x80 => {
                pos = 1;
                b as usize
            }
            _ => get_varint(data, &mut pos)? as usize,
        };
        for (c, slot) in cols.iter_mut().enumerate() {
            let Some(col) = slot.as_mut() else {
                if c < n {
                    skip(data, &mut pos)?;
                }
                continue;
            };
            let present = c < n;
            if present && !ext {
                let t = data.get(pos).copied();
                if t == Some(tag::NULL) {
                    pos += 1;
                    col.nulls[r] = true;
                    col.data.set_default(r);
                    continue;
                }
                if put_typed(col, r, own, data, &mut pos, buf) {
                    continue;
                }
            }
            if ext || (present && data.get(pos) == Some(&tag::NULL)) {
                if present {
                    skip(data, &mut pos)?;
                }
                col.nulls[r] = true;
                col.data.set_default(r);
                continue;
            }
            let v = match present {
                true => super::row::decode_value(data, &mut pos)?,
                false => missing.get(c).cloned().unwrap_or(Value::Null),
            };
            col.nulls[r] = v.is_null();
            col.data.set_default(r);
            col.odd.push((r as u32, v));
        }
        Ok(())
    };
    if all_left {
        for (r, i) in rows.enumerate() {
            row(r, i)?;
        }
    } else {
        for r in left {
            row(r as usize, rows.start + r as usize)?;
        }
    }
    bufs[0] = Bytes::from(std::mem::take(own));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::row;
    use crate::types::numeric::Numeric;

    /// Every column decoded gives what `decode_row` gives, in and out of block buffers,
    /// with NULLs, odd values, rows stored before columns were added and skipped columns.
    #[test]
    fn columns_match_decoded_rows() {
        let num = |s: &str| Value::numeric(Numeric::parse_pg(s).unwrap());
        let types = [
            DataType::Int2,
            DataType::Int4,
            DataType::Int8,
            DataType::Float8,
            DataType::Numeric(-1),
            DataType::Bool,
            DataType::Date,
            DataType::Text,
            DataType::Interval(-1),
            DataType::Int4,
        ];
        let rows: Vec<Vec<Value>> = vec![
            vec![Value::Int2(-3), Value::Int4(7), Value::Int8(1 << 40), Value::Float8(2.5), num("12.340"), Value::Bool(true), Value::Date(9), Value::Text("héllo".into()), Value::Null, Value::Int4(1)],
            vec![Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null],
            vec![Value::Int2(1), Value::Int4(-1), Value::Int8(0), Value::Float8(-0.0), num("123456789012345678901234567890123456789012.5"), Value::Bool(false), Value::Date(-1), Value::Text("".into()), Value::Null],
            vec![Value::Int2(2), Value::Int4(2)],
        ];
        let missing = vec![Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Text("m".into()), Value::Null, Value::Int4(42)];
        let stored = |r: &[Value]| {
            let mut b = vec![row::ROW_HEADER, 5];
            b.extend(row::encode_row(r));
            b
        };
        let mut block = Vec::new();
        let mut spans = Vec::new();
        for r in &rows {
            let s = stored(r);
            spans.push((block.len(), s.len()));
            block.extend(s);
        }
        let block = Bytes::from(block);
        let mut batch = RowBatch::default();
        for (i, (o, l)) in spans.iter().enumerate() {
            if i == 1 {
                batch.push_copy(b"k", &block[*o..o + l]);
            } else {
                batch.push_in(b"k", &block, *o, *l);
            }
        }
        for need in [vec![true; 10], vec![false, true, false, false, true, false, false, true, false, true]] {
            let mut out = ColBatch::default();
            decode_cols(&batch, 0..batch.len(), &need, &types, &missing, &mut out).unwrap();
            assert_eq!(out.len, rows.len());
            for (i, r) in rows.iter().enumerate() {
                for (c, ty) in types.iter().enumerate() {
                    let Some(col) = &out.cols[c] else {
                        assert!(!need[c]);
                        continue;
                    };
                    let want = r.get(c).cloned().unwrap_or_else(|| missing[c].clone());
                    assert_eq!(format!("{:?}", col.value(&out, i, ty).unwrap()), format!("{want:?}"), "row {i} col {c}");
                }
            }
        }
    }

    /// Numerics of every stored length (one word or more, at the row's end or not) decode
    /// to their values, both signs at each length's limits.
    #[test]
    fn numerics_of_every_length() {
        let mut vals: Vec<String> = ["0", "-0.01", "1", "-1", "127", "-128", "128", "-129", "0.00"].map(String::from).to_vec();
        for k in 1..=40 {
            for s in ["", "-"] {
                vals.push(format!("{s}{}", "9".repeat(k)));
                vals.push(format!("{s}1{}.5", "0".repeat(k)));
            }
        }
        let num = |s: &str| Value::numeric(Numeric::parse_pg(s).unwrap());
        let types = [DataType::Numeric(-1), DataType::Int4];
        for last in [false, true] {
            let mut batch = RowBatch::default();
            for v in &vals {
                let r: Vec<Value> = if last { vec![num(v)] } else { vec![num(v), Value::Int4(1)] };
                let mut b = vec![row::ROW_HEADER, 5];
                b.extend(row::encode_row(&r));
                batch.push_copy(b"k", &b);
            }
            let mut out = ColBatch::default();
            decode_cols(&batch, 0..batch.len(), &[true, false], &types, &[Value::Null, Value::Null], &mut out).unwrap();
            let col = out.cols[0].as_ref().unwrap();
            for (i, v) in vals.iter().enumerate() {
                assert_eq!(format!("{:?}", col.value(&out, i, &types[0]).unwrap()), format!("{:?}", num(v)), "{v} last={last}");
            }
        }
    }

    /// The typed walk (and the rows it leaves to the general walk: NULLs, long strings,
    /// numerics past a word or another tag, rows outside a block buffer or without their
    /// last columns, values of types it steps over by tag) gives what `decode_row` gives,
    /// for masks taking and skipping every column.
    #[test]
    fn typed_walk_matches_decoded_rows() {
        let types = [
            DataType::Int4,
            DataType::Numeric(-1),
            DataType::Text,
            DataType::Date,
            DataType::Int8,
            DataType::Numeric(-1),
            DataType::Bpchar(-1),
            DataType::Float8,
            DataType::Int2,
            DataType::Bool,
            DataType::Interval(-1),
            DataType::Bytea,
            DataType::Float4,
            DataType::Timestamp(-1),
        ];
        let num = |s: String| Value::numeric(Numeric::parse_pg(&s).unwrap());
        let mut rows = Vec::new();
        for i in 0..400i64 {
            let null = |k: i64| i % k == 0;
            let mut r = vec![
                if null(17) { Value::Null } else { Value::Int4((i * 7919 - 1000) as i32) },
                if null(13) { Value::Null } else { num(format!("{}{}.{}", if i % 2 == 0 { "-" } else { "" }, "9".repeat((i % 25) as usize + 1), i % 100)) },
                if null(11) { Value::Null } else { Value::Text("x".repeat((i * 37 % 200) as usize).into()) },
                Value::Date(i as i32 - 150),
                if null(19) { Value::Null } else { Value::Int8(i * 1_000_000_007) },
                if null(7) { Value::Null } else { num(format!("{}.{}", i, "0".repeat((i % 4) as usize))) },
                Value::Text(["ab ", "c  ", "   "][(i % 3) as usize].into()),
                if i % 23 == 0 { Value::Int4(5) } else { Value::Float8(i as f64 / 3.0) },
                Value::Int2((i % 300) as i16),
                if null(5) { Value::Null } else { Value::Bool(i % 3 == 0) },
                if null(3) { Value::Null } else { Value::Interval(crate::types::Interval { months: i as i32, days: 1, micros: 7 }) },
                if null(29) { Value::Null } else { Value::bytea(vec![i as u8; (i % 140) as usize]) },
                Value::Float4(i as f32 * 0.5),
                Value::Timestamp(i * 86_400_000_000),
            ];
            // (Rows stored before the last columns were added.)
            if i % 31 == 0 {
                r.truncate(9);
            }
            rows.push(r);
        }
        let missing: Vec<Value> = (0..types.len()).map(|c| if c == 12 { Value::Float4(1.5) } else { Value::Null }).collect();
        let mut block = Vec::new();
        let mut spans = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let mut b = vec![if i % 2 == 0 { row::ROW_HEADER } else { row::ROW_HEADER_TID }, 5];
            if i % 2 == 1 {
                b.extend([3, 0x81, 1]);
            }
            b.extend(row::encode_row(r));
            spans.push((block.len(), b.len()));
            block.extend(b);
        }
        let block = Bytes::from(block);
        let mut batch = RowBatch::default();
        for (i, (o, l)) in spans.iter().enumerate() {
            if i % 10 == 9 {
                batch.push_copy(b"k", &block[*o..o + l]);
            } else {
                batch.push_in(b"k", &block, *o, *l);
            }
        }
        for mask in [u32::MAX, 0b10_0100_1001_0010, 0b00_0000_1010, 0b01_0010_0100, 0b00_1000_0000, 0b00_0010_0000, 0b10_1000_0000_1000, 0b01_0000_0000_0001] {
            let need: Vec<bool> = (0..types.len()).map(|c| mask >> c & 1 == 1).collect();
            let mut out = ColBatch::default();
            // (Twice into one batch: slots kept from batch to batch.)
            for _ in 0..2 {
                decode_cols(&batch, 0..batch.len(), &need, &types, &missing, &mut out).unwrap();
            }
            for (i, r) in rows.iter().enumerate() {
                for (c, ty) in types.iter().enumerate() {
                    if let Some(col) = &out.cols[c] {
                        let want = r.get(c).cloned().unwrap_or_else(|| missing[c].clone());
                        assert_eq!(format!("{:?}", col.value(&out, i, ty).unwrap()), format!("{want:?}"), "mask {mask:b} row {i} col {c}");
                        if want.is_null() {
                            assert!(col.nulls[i], "mask {mask:b} row {i} col {c}");
                        }
                    }
                }
            }
        }
    }

    /// `trim_spaces` is a byte-at-a-time trim of trailing spaces, for every length and count
    /// of spaces (spaces inside kept, a tab not a space).
    #[test]
    fn trim_spaces_of_every_length() {
        for len in 0..40usize {
            for sp in 0..=len {
                for inner in *b"a \t" {
                    let mut x = vec![b'x'; len - sp];
                    if let Some(m) = x.get_mut((len - sp) / 2) {
                        *m = inner;
                    }
                    x.extend(std::iter::repeat_n(b' ', sp));
                    let mut want = &x[..];
                    while let [rest @ .., b' '] = want {
                        want = rest;
                    }
                    assert_eq!(trim_spaces(&x), want, "{x:?}");
                }
            }
        }
    }

    /// Decode cost on lineitem-shaped rows (TPC-H q06's and q01's columns), for perf stat:
    /// `A251_REPS=<n> cargo test --profile verify --lib storage::cols::tests::decode_cost -- --ignored`.
    #[test]
    #[ignore]
    fn decode_cost() {
        let reps: usize = std::env::var("A251_REPS").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
        let num = |x: i64| Value::numeric(Numeric::from_parts(crate::types::decint::DecInt::from(x as i128), 2));
        let types = [
            DataType::Int4, DataType::Int4, DataType::Int4, DataType::Int4,
            DataType::Numeric(983046), DataType::Numeric(983046), DataType::Numeric(983046), DataType::Numeric(983046),
            DataType::Bpchar(-1), DataType::Bpchar(-1), DataType::Date, DataType::Date, DataType::Date,
            DataType::Bpchar(-1), DataType::Bpchar(-1), DataType::Varchar(-1),
        ];
        let mut block = Vec::new();
        let mut spans = Vec::new();
        for i in 0..2048i64 {
            let r = vec![
                Value::Int4(i as i32 * 7), Value::Int4(i as i32 * 13 % 200000), Value::Int4(i as i32 % 10000), Value::Int4((i % 7) as i32),
                num((i % 50 + 1) * 100), num(90000 + i * 37 % 10_000_000), num(i % 11), num(i % 9),
                Value::Text(["A", "N", "R"][(i % 3) as usize].into()), Value::Text(["F", "O"][(i % 2) as usize].into()),
                Value::Date(8000 + (i % 2500) as i32), Value::Date(8030 + (i % 2500) as i32), Value::Date(8010 + (i % 2500) as i32),
                Value::Text("DELIVER IN PERSON        ".into()), Value::Text("TRUCK     ".into()), Value::Text("final requests sleep furiously".into()),
            ];
            let mut b = vec![row::ROW_HEADER, 5];
            b.extend(row::encode_row(&r));
            spans.push((block.len(), b.len()));
            block.extend(b);
        }
        let block = Bytes::from(block);
        let mut batch = RowBatch::default();
        for (o, l) in &spans {
            batch.push_in(b"k", &block, *o, *l);
        }
        let missing = vec![Value::Null; types.len()];
        let q06: Vec<bool> = (0..16).map(|c| matches!(c, 4 | 5 | 6 | 10)).collect();
        let q01: Vec<bool> = (0..16).map(|c| (4..=10).contains(&c)).collect();
        let mut out = ColBatch::default();
        let t = std::time::Instant::now();
        for _ in 0..reps {
            decode_cols(&batch, 0..batch.len(), &q06, &types, &missing, &mut out).unwrap();
            decode_cols(&batch, 0..batch.len(), &q01, &types, &missing, &mut out).unwrap();
        }
        eprintln!("decode_cost: {:.1} ns a row (q06 + q01 masks)", t.elapsed().as_nanos() as f64 / (reps * 2 * 2048).max(1) as f64);
    }
}
