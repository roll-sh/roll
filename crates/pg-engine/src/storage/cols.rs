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

    fn push_default(&mut self) {
        match self {
            ColData::I32(v) | ColData::Date(v) => v.push(0),
            ColData::I64(v) => v.push(0),
            ColData::F64(v) => v.push(0.0),
            ColData::Num(v) => v.push((0, 0)),
            ColData::Bool(v) => v.push(false),
            ColData::Bytes(v) => v.push((0, 0, 0)),
            ColData::Values(v) => v.push(Value::Null),
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

/// The value at `data[*pos]` into `col`'s typed slot, `*pos` moved past it (false: it doesn't
/// fit, `*pos` unchanged: odd). A string's bytes stay where they are: in `buf` (the block
/// buffer `data` is in, at `base`), else copied into `own` (buffer 0).
#[inline]
fn push_typed(col: &mut Col, own: &mut Vec<u8>, data: &[u8], pos: &mut usize, buf: Option<(u32, usize)>) -> bool {
    let p = *pos;
    let t = data[p];
    macro_rules! fixed {
        ($v:expr, $ty:ty, $n:expr, $conv:expr) => {{
            let Some(x) = data.get(p + 1..p + 1 + $n) else { return false };
            let x = <$ty>::from_le_bytes(x.try_into().unwrap());
            $v.push($conv(x));
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
            v.push(t == tag::TRUE);
            *pos = p + 1;
        }
        (ColData::Num(v), tag::NUMERIC) => {
            // (Scale and length in a byte each, the common case; `peek_num` otherwise.)
            let (sc, n) = match (data.get(p + 1), data.get(p + 2)) {
                (Some(&sc), Some(&n)) if sc < 0x80 && n < 0x80 => (sc as u32, n as usize),
                _ => {
                    let Some(Some(x)) = super::row::peek_num(data, p) else { return false };
                    let mut q = p + 1;
                    if get_varint(data, &mut q).is_err() {
                        return false;
                    }
                    let Ok(len) = get_varint(data, &mut q) else { return false };
                    v.push(x);
                    *pos = q + len as usize;
                    return true;
                }
            };
            let Some(x) = data.get(p + 3..p + 3 + n) else { return false };
            // (Two's complement, little-endian, none longer than an i128's.)
            if n > 16 {
                return false;
            }
            let fill = if n > 0 && x[n - 1] & 0x80 != 0 { 0xFF } else { 0 };
            let mut w = [fill; 16];
            w[..n].copy_from_slice(x);
            v.push((i128::from_le_bytes(w), sc));
            *pos = p + 3 + n;
        }
        (ColData::Bytes(v), tag::TEXT | tag::BYTEA) => {
            let mut q = p + 1;
            let Ok(n) = get_varint(data, &mut q) else { return false };
            let e = q + n as usize;
            if e > data.len() {
                return false;
            }
            match buf {
                Some((b, base)) => v.push((b, (base + q) as u32, (base + e) as u32)),
                None => {
                    let s = own.len() as u32;
                    own.extend_from_slice(&data[q..e]);
                    v.push((0, s, own.len() as u32));
                }
            }
            *pos = e;
        }
        (ColData::Values(v), _) => {
            let mut q = p;
            match super::row::decode_value(data, &mut q) {
                Ok(x) => v.push(x),
                Err(_) => return false,
            }
            *pos = q;
        }
        _ => return false,
    }
    true
}

/// `row::skip_value_fast` with numerics stepped over in place too.
#[inline]
fn skip(data: &[u8], pos: &mut usize) -> Result<()> {
    let p = *pos;
    if data.get(p) == Some(&tag::NUMERIC)
        && let (Some(&sc), Some(&n)) = (data.get(p + 1), data.get(p + 2))
        && sc < 0x80
        && n < 0x80
        && p + 3 + (n as usize) <= data.len()
    {
        *pos = p + 3 + n as usize;
        return Ok(());
    }
    super::row::skip_value_fast(data, pos)
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
    out.cols.resize_with(ncols.max(out.cols.len()), || None);
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
    }
    out.len = rows.len();
    out.bufs.clear();
    out.ext.clear();
    out.own.clear();
    // (Buffer 0 is `own`, put in place at the end; the block buffers rows are in follow,
    // each once while consecutive rows share it.)
    out.bufs.push(Bytes::new());
    let upto = need.iter().take(ncols).rposition(|b| *b).map_or(0, |c| c + 1);
    for (r, i) in rows.enumerate() {
        let (bytes, inbuf) = batch.value_in(i);
        let ext = matches!(bytes.first(), Some(&(ROW_HEADER_EXT | ROW_HEADER_TID_EXT)));
        if ext {
            out.ext.push(r as u32);
        }
        let data = row_values(bytes).ok_or_else(|| crate::error::internal("stored row without a header"))?;
        let buf = inbuf.map(|(b, _)| {
            let last = out.bufs.last().unwrap();
            if out.bufs.len() == 1 || last.as_ptr() != b.as_ptr() || last.len() != b.len() {
                out.bufs.push(b.clone());
            }
            ((out.bufs.len() - 1) as u32, data.as_ptr() as usize - b.as_ptr() as usize)
        });
        let mut pos = 0;
        let n = get_varint(data, &mut pos)? as usize;
        for c in 0..upto {
            let Some(col) = out.cols[c].as_mut() else {
                if c < n {
                    skip(data, &mut pos)?;
                }
                continue;
            };
            let present = c < n;
            if present && !ext && data.get(pos) != Some(&tag::NULL) && push_typed(col, &mut out.own, data, &mut pos, buf) {
                col.nulls.push(false);
                continue;
            }
            if ext || (present && data.get(pos) == Some(&tag::NULL)) {
                if present {
                    skip(data, &mut pos)?;
                }
                col.nulls.push(true);
                col.data.push_default();
                continue;
            }
            let v = match present {
                true => super::row::decode_value(data, &mut pos)?,
                false => missing.get(c).cloned().unwrap_or(Value::Null),
            };
            col.nulls.push(v.is_null());
            col.data.push_default();
            col.odd.push((r as u32, v));
        }
    }
    out.bufs[0] = Bytes::from(std::mem::take(&mut out.own));
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
}
