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

    fn clear(&mut self) {
        match self {
            ColData::I32(v) | ColData::Date(v) => v.clear(),
            ColData::I64(v) => v.clear(),
            ColData::F64(v) => v.clear(),
            ColData::Num(v) => v.clear(),
            ColData::Bool(v) => v.clear(),
            ColData::Bytes(v) => v.clear(),
            ColData::Values(v) => v.clear(),
        }
    }

    /// `n` slots, each a default (a NULL's, an odd row's).
    fn fill_default(&mut self, n: usize) {
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
/// Up to 8 bytes with 8 of the row from there are one word load, sign-extended.
#[inline]
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
fn skip(data: &[u8], pos: &mut usize) -> Result<()> {
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
                col.data.clear();
                col.nulls.clear();
                col.odd.clear();
            }
            _ => *slot = Some(Col { data: want, nulls: Vec::new(), odd: Vec::new() }),
        }
        let col = slot.as_mut().unwrap();
        col.data.fill_default(len);
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
    // (The batch's parts borrowed apart: the loop keeps them in registers.)
    let ColBatch { cols, bufs, ext: exts, own, .. } = out;
    let cols = &mut cols[..upto];
    for (r, i) in rows.enumerate() {
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
                continue;
            }
            let v = match present {
                true => super::row::decode_value(data, &mut pos)?,
                false => missing.get(c).cloned().unwrap_or(Value::Null),
            };
            col.nulls[r] = v.is_null();
            col.odd.push((r as u32, v));
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
}
