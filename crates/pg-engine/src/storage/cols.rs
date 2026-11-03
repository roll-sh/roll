//! Columns decoded from a batch of stored rows (`decode_cols`): the values a scan's
//! filters, projections and aggregates read, a typed vector per column instead of a
//! `Vec<Value>` per row. Strings stay in the blocks holding them (a span of a buffer the
//! batch references once), numbers are read in place (`row::peek_num`'s encoding).
//!
//! A stored row is a header (xmin, maybe a TID), a value count, then tagged values
//! (`row::tag`); there is no columnar layout, so each row is walked once up to the last
//! column needed. A value a column's typed vector can't hold (a numeric past an i128, a
//! value of an unexpected tag) goes to the column's `odd` list; a row with values stored
//! out of line is listed in `ext` with its columns left NULL, for the scan to read whole
//! (`toast`) if a filter keeps it.
use super::batch::RowBatch;
use super::row::{self, tag};
use super::sst::get_varint;
use crate::error::Result;
use crate::types::{DataType, Value};
use bytes::Bytes;

/// A column's values, by row.
pub enum ColData {
    /// int2, int4 (widened).
    I32(Vec<i32>),
    /// int8.
    I64(Vec<i64>),
    /// float4 (widened), float8.
    F64(Vec<f64>),
    /// numeric: unscaled value and scale.
    Num(Vec<(i128, u32)>),
    Bool(Vec<bool>),
    /// date: days from 2000-01-01.
    Date(Vec<i32>),
    /// text, varchar, bpchar, name, bytea: (buffer, start, end) of the bytes in
    /// `ColBatch::bufs`.
    Bytes(Vec<(u32, u32, u32)>),
    /// Every other type.
    Values(Vec<Value>),
}

pub struct Col {
    pub data: ColData,
    /// NULL rows (a NULL's slot in `data` holds a default).
    pub nulls: Vec<bool>,
    /// Rows whose value `data` doesn't hold, with it (their slots hold a default).
    pub odd: Vec<(u32, Value)>,
}

/// Columns of up to a batch of rows (`decode_cols`); the buffers are kept from batch to
/// batch.
#[derive(Default)]
pub struct ColBatch {
    pub len: usize,
    /// By column of the table; None: not needed.
    pub cols: Vec<Option<Col>>,
    /// The buffers `ColData::Bytes` spans are in (blocks referenced once per batch, and one
    /// buffer of copied values).
    pub bufs: Vec<Bytes>,
    /// Rows with values stored out of line (their needed columns NULL here).
    pub ext: Vec<u32>,
    copied: Vec<u8>,
}

fn data_for(ty: &DataType) -> ColData {
    use DataType as T;
    match ty {
        T::Int2 | T::Int4 => ColData::I32(Vec::new()),
        T::Int8 => ColData::I64(Vec::new()),
        T::Float4 | T::Float8 => ColData::F64(Vec::new()),
        T::Numeric(_) => ColData::Num(Vec::new()),
        T::Bool => ColData::Bool(Vec::new()),
        T::Date => ColData::Date(Vec::new()),
        T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name | T::Bytea => ColData::Bytes(Vec::new()),
        _ => ColData::Values(Vec::new()),
    }
}

impl ColData {
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

    /// A slot holding no value of its own (a NULL's, an odd value's).
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
    /// Row `i`'s value as a `Value` (what a row-at-a-time consumer reads).
    pub fn value(&self, b: &ColBatch, i: usize, ty: &DataType) -> Result<Value> {
        if self.nulls[i] {
            return Ok(Value::Null);
        }
        if let Some((_, v)) = self.odd.iter().find(|(r, _)| *r as usize == i) {
            return Ok(v.clone());
        }
        Ok(match &self.data {
            ColData::I32(v) if matches!(ty, DataType::Int2) => Value::Int2(v[i] as i16),
            ColData::I32(v) => Value::Int4(v[i]),
            ColData::I64(v) => Value::Int8(v[i]),
            ColData::F64(v) if matches!(ty, DataType::Float4) => Value::Float4(v[i] as f32),
            ColData::F64(v) => Value::Float8(v[i]),
            ColData::Num(v) => {
                let (n, s) = v[i];
                Value::numeric(crate::types::numeric::Numeric::from_parts(crate::types::decint::DecInt::from_signed_bytes_le(&n.to_le_bytes()), s))
            }
            ColData::Bool(v) => Value::Bool(v[i]),
            ColData::Date(v) => Value::Date(v[i]),
            ColData::Bytes(_) if matches!(ty, DataType::Bytea) => Value::bytea(self.bytes(b, i).to_vec()),
            ColData::Bytes(_) => Value::Text(
                std::str::from_utf8(self.bytes(b, i)).map_err(|_| crate::error::internal("invalid UTF-8 in stored row"))?.into(),
            ),
            ColData::Values(v) => v[i].clone(),
        })
    }

    /// Row `i`'s bytes of a `Bytes` column (empty for a NULL or another column).
    pub fn bytes<'b>(&self, b: &'b ColBatch, i: usize) -> &'b [u8] {
        match &self.data {
            ColData::Bytes(v) => {
                let (buf, s, e) = v[i];
                b.bufs.get(buf as usize).map_or(&[][..], |x| &x[s as usize..e as usize])
            }
            _ => &[],
        }
    }
}

/// A copied value's span until the batch's copies become a buffer.
const COPIED: u32 = u32::MAX;

/// The values after a stored row's header (None: not a row), and whether it has values
/// stored out of line.
fn row_values(bytes: &[u8]) -> Option<(&[u8], bool)> {
    let (n, ext) = match *bytes.first()? {
        row::ROW_HEADER => (1, false),
        row::ROW_HEADER_EXT => (1, true),
        row::ROW_HEADER_TID => (3, false),
        row::ROW_HEADER_TID_EXT => (3, true),
        _ => return None,
    };
    let mut pos = 1;
    for _ in 0..n {
        pos += bytes.get(pos..)?.iter().position(|b| *b < 0x80)? + 1;
    }
    Some((bytes.get(pos..)?, ext))
}

/// Rows `rows` of `batch` decoded into `out`: the columns `need` marks (by column of the
/// table, of types `types`; a row stored before a column was added has its `missing`
/// value). `out`'s vectors are reused.
pub fn decode_cols(
    batch: &RowBatch,
    rows: std::ops::Range<usize>,
    need: &[bool],
    types: &[DataType],
    missing: &[Value],
    out: &mut ColBatch,
) -> Result<()> {
    out.len = 0;
    out.bufs.clear();
    out.ext.clear();
    out.copied.clear();
    out.cols.resize_with(types.len(), || None);
    for (i, c) in out.cols.iter_mut().enumerate() {
        if !need.get(i).copied().unwrap_or(false) {
            *c = None;
            continue;
        }
        let c = c.get_or_insert_with(|| Col { data: data_for(&types[i]), nulls: Vec::new(), odd: Vec::new() });
        c.data.clear();
        c.nulls.clear();
        c.odd.clear();
    }
    let last = need.iter().rposition(|n| *n).map_or(0, |l| l + 1).min(types.len());
    for r in rows {
        let (bytes, buf) = batch.value_in(r);
        let Some((data, ext)) = row_values(bytes) else {
            return Err(crate::error::internal("invalid stored row"));
        };
        let at = out.len as u32;
        out.len += 1;
        let mut pos = 0;
        let n = get_varint(data, &mut pos)? as usize;
        // (The buffer holding the row, referenced once while consecutive rows share it.)
        let buf_ix = match buf {
            Some((b, off)) => {
                if out.bufs.last().is_none_or(|l| l.as_ptr() != b.as_ptr() || l.len() != b.len()) {
                    out.bufs.push(b.clone());
                }
                Some((out.bufs.len() as u32 - 1, off + (bytes.len() - data.len())))
            }
            None => None,
        };
        if ext {
            out.ext.push(at);
        }
        for col in 0..last {
            decode_one(out, col, n, at, ext, buf_ix, data, &mut pos, missing)?;
        }
    }
    if !out.copied.is_empty() {
        let ix = out.bufs.len() as u32;
        out.bufs.push(Bytes::copy_from_slice(&out.copied));
        for c in out.cols.iter_mut().flatten() {
            if let ColData::Bytes(v) = &mut c.data {
                for s in v.iter_mut().filter(|s| s.0 == COPIED) {
                    s.0 = ix;
                }
            }
        }
    }
    Ok(())
}

/// Column `col` of a row (`n` values, at `pos` in `data`) into `out`, `pos` past it.
#[allow(clippy::too_many_arguments)]
fn decode_one(
    out: &mut ColBatch,
    col: usize,
    n: usize,
    at: u32,
    ext: bool,
    buf_ix: Option<(u32, usize)>,
    data: &[u8],
    pos: &mut usize,
    missing: &[Value],
) -> Result<()> {
    let Some(c) = out.cols[col].as_mut() else {
        if col < n {
            row::skip_value_fast(data, pos)?;
        }
        return Ok(());
    };
    if col >= n {
        // (Stored before the column was added.)
        let m = missing.get(col).cloned().unwrap_or(Value::Null);
        c.nulls.push(m.is_null());
        c.data.push_default();
        if !m.is_null() {
            c.odd.push((at, m));
        }
        return Ok(());
    }
    let p0 = *pos;
    let t = data[p0];
    if t == tag::NULL || ext {
        c.nulls.push(true);
        c.data.push_default();
        return row::skip_value_fast(data, pos);
    }
    c.nulls.push(false);
    let fixed = |n: usize| data.get(p0 + 1..p0 + 1 + n);
    let done = match (&mut c.data, t) {
        (ColData::I32(v), tag::INT2) => fixed(2).map(|b| v.push(i16::from_le_bytes([b[0], b[1]]) as i32)),
        (ColData::I32(v), tag::INT4) | (ColData::Date(v), tag::DATE) => {
            fixed(4).map(|b| v.push(i32::from_le_bytes([b[0], b[1], b[2], b[3]])))
        }
        (ColData::I64(v), tag::INT8) => fixed(8).map(|b| v.push(i64::from_le_bytes(b.try_into().unwrap()))),
        (ColData::F64(v), tag::FLOAT8) => fixed(8).map(|b| v.push(f64::from_le_bytes(b.try_into().unwrap()))),
        (ColData::F64(v), tag::FLOAT4) => fixed(4).map(|b| v.push(f32::from_le_bytes(b.try_into().unwrap()) as f64)),
        (ColData::Bool(v), tag::TRUE | tag::FALSE) => {
            v.push(t == tag::TRUE);
            Some(())
        }
        (ColData::Num(v), tag::NUMERIC) => match row::peek_num(data, p0) {
            Some(Some(x)) => {
                v.push(x);
                Some(())
            }
            _ => None,
        },
        (ColData::Bytes(v), tag::TEXT | tag::BYTEA) => {
            let mut p = p0 + 1;
            let len = get_varint(data, &mut p)? as usize;
            let s = data.get(p..p + len).ok_or_else(|| crate::error::internal("truncated row"))?;
            match buf_ix {
                Some((b, off)) => v.push((b, (off + p) as u32, (off + p + len) as u32)),
                None => {
                    let st = out.copied.len() as u32;
                    out.copied.extend_from_slice(s);
                    v.push((COPIED, st, out.copied.len() as u32));
                }
            }
            Some(())
        }
        (ColData::Values(v), _) => {
            let mut p = p0;
            v.push(row::decode_value(data, &mut p)?);
            Some(())
        }
        _ => None,
    };
    if done.is_none() {
        // (A value the typed vector doesn't hold.)
        let mut p = p0;
        let v = row::decode_value(data, &mut p)?;
        c.data.push_default();
        c.odd.push((at, v));
    }
    row::skip_value_fast(data, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
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
