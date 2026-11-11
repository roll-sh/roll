//! Out-of-line storage of large values: the engine's TOAST.
//!
//! A row's value whose encoding is large (`exec::toast` decides which) is stored apart
//! from its row, under the table's out-of-line key space, and the row keeps a pointer to
//! it (`row::EXTERNAL`: the writing transaction, a sequence number and the value's encoded
//! length). So:
//! - a scan that doesn't read the column never touches the value's bytes;
//! - an UPDATE that keeps the value keeps the pointer, and the value isn't written again;
//! - a value's key starts with its row's key, so the values of a table sort as its rows do.
//!
//! Keys: `table | SPACE | row key suffix | xid (8 bytes) | seq (8 bytes)`, big-endian. A
//! value is written once under a key of its own and never changed; it is deleted with the
//! last row version pointing at it (an UPDATE that changes it, a DELETE, TRUNCATE).
//!
//! Reads give rows with their values put back (`splice`): the store's own reads
//! (`Store::get`, `scan`, ...) and the transaction's (`Txn::get`, `TxnScan`), unless a
//! reader asks for the stored row as it is and puts back only the values it reads. A value
//! is read as its newest version (a value never changes, so every version of its key is the
//! value), or, when that is a deletion, as of the reader's snapshot: the row the reader has
//! is an older version, which the snapshot sees, so it sees the value too. The newest version
//! also finds a value committed after the reader's snapshot, which a READ COMMITTED UPDATE
//! of a row's newest version may keep.
//!
//! What PostgreSQL makes visible of TOAST (pg_column_size, pg_column_compression, the TOAST
//! relation's size and chunks) comes from the size model (`exec::sizing`), not from this.

use super::View;
use super::keys::{index_prefix, prefix_end};
use super::row::{EXTERNAL, ROW_HEADER, ROW_HEADER_EXT, ROW_HEADER_TID, ROW_HEADER_TID_EXT, skip_value};
use super::sst::{get_varint, put_varint};
use crate::error::{Result, internal};
use bytes::Bytes;

/// The out-of-line values' key space of a table: where an index's id goes in an index
/// entry's key (no index has it: index ids are their oids).
pub const SPACE: u32 = u32::MAX;

/// A timestamp past every commit: a key's newest version.
const NEWEST: u64 = u64::MAX >> 1;

/// Out-of-line values read from the store (tests read it; a cursor's reads count too:
/// `txn::toast::Reader`).
#[doc(hidden)]
pub static VALUES_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The key prefix of table `table`'s out-of-line values.
pub fn prefix(table: u32) -> Vec<u8> {
    index_prefix(table, SPACE)
}

/// Whether `key` is a table row's (a table's oid, index id 0; system keys have oid 0).
pub fn is_row_key(key: &[u8]) -> bool {
    key.len() > 8 && key[..4] != [0; 4] && key[4..8] == [0; 4]
}

/// Whether `key` is an out-of-line value's (a table's oid, index id `SPACE`).
pub fn is_value_key(key: &[u8]) -> bool {
    key.len() > 8 && key[..4] != [0; 4] && key[4..8] == SPACE.to_be_bytes()
}

/// Whether some table row's key may lie in [start, end) (table `t`'s rows are in
/// [`t | 0`, `t | 1`), and no table has oid 0).
pub fn may_hold_rows(start: &[u8], end: &[u8]) -> bool {
    let t = match start.get(..4) {
        Some(b) => u32::from_be_bytes(b.try_into().unwrap()),
        None => 0,
    };
    // (The first table whose rows can come at or after `start`.)
    let t = if start < &index_prefix(t, 1)[..] { t } else { t.saturating_add(1) };
    index_prefix(t.max(1), 0).as_slice() < end
}

/// Whether the stored row `v` points at out-of-line values.
pub fn has_external(v: &[u8]) -> bool {
    matches!(v.first(), Some(&(ROW_HEADER_EXT | ROW_HEADER_TID_EXT)))
}

/// Which value a pointer names: the transaction that wrote it and its number there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValueId {
    pub xid: u64,
    pub seq: u64,
}

/// A pointer to value `id` of `len` encoded bytes, as a row stores it.
pub fn put_pointer(out: &mut Vec<u8>, id: ValueId, len: usize) {
    out.push(EXTERNAL);
    put_varint(out, id.xid);
    put_varint(out, id.seq);
    put_varint(out, len as u64);
}

/// The pointer at `*pos` (its tag included), moving `pos` past it: the value's id and
/// encoded length. None when the value there isn't a pointer.
pub fn read_pointer(b: &[u8], pos: &mut usize) -> Result<Option<(ValueId, usize)>> {
    if b.get(*pos) != Some(&EXTERNAL) {
        return Ok(None);
    }
    *pos += 1;
    let xid = get_varint(b, pos)?;
    let seq = get_varint(b, pos)?;
    let len = get_varint(b, pos)? as usize;
    Ok(Some((ValueId { xid, seq }, len)))
}

/// The key of value `id` of the row stored under `row_key`.
pub fn value_key(row_key: &[u8], id: ValueId) -> Vec<u8> {
    let mut k = Vec::with_capacity(row_key.len() + 16);
    k.extend_from_slice(&row_key[..4]);
    k.extend_from_slice(&SPACE.to_be_bytes());
    k.extend_from_slice(&row_key[8..]);
    k.extend_from_slice(&id.xid.to_be_bytes());
    k.extend_from_slice(&id.seq.to_be_bytes());
    k
}

/// A key range holding the out-of-line values of the rows whose keys are in [start, end),
/// when that range is (part of) one table's rows.
pub fn value_range(start: &[u8], end: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if start.len() < 8 || start[..4] == [0; 4] || start[4..8] != [0; 4] {
        return None;
    }
    let p = prefix(u32::from_be_bytes(start[..4].try_into().unwrap()));
    let mut lo = p.clone();
    lo.extend_from_slice(&start[8..]);
    // (A row key's values sort after it; those of keys with `end` as a prefix are in too.)
    let hi = if end.len() > 8 && end[..8] == start[..8] {
        let mut e = p;
        e.extend_from_slice(&end[8..]);
        prefix_end(&e)
    } else {
        prefix_end(&p)
    };
    Some((lo, hi))
}

/// The length of a stored row's header (the header byte, its xmin and TID if any).
fn header_len(b: &[u8]) -> Result<usize> {
    let tid = match b.first() {
        Some(&(ROW_HEADER | ROW_HEADER_EXT)) => false,
        Some(&(ROW_HEADER_TID | ROW_HEADER_TID_EXT)) => true,
        _ => return Ok(0),
    };
    let mut pos = 1;
    get_varint(b, &mut pos)?;
    if tid {
        get_varint(b, &mut pos)?;
        get_varint(b, &mut pos)?;
    }
    Ok(pos)
}

/// The row `bytes` stored under `key` with the out-of-line values of the columns `need`
/// marks (None: all) put back in place of their pointers, read by `fetch` (by key). Its
/// header says whether pointers are left.
pub fn splice(
    key: &[u8],
    bytes: &[u8],
    need: Option<&[bool]>,
    fetch: &mut dyn FnMut(&[u8]) -> Result<Bytes>,
) -> Result<Vec<u8>> {
    let mut pos = header_len(bytes)?;
    let n = get_varint(bytes, &mut pos)? as usize;
    let mut out = Vec::with_capacity(bytes.len() + 256);
    out.extend_from_slice(&bytes[..pos]);
    let mut left = false;
    for i in 0..n {
        let at = pos;
        match read_pointer(bytes, &mut pos)? {
            Some((id, _)) if need.is_none_or(|m| m.get(i).copied().unwrap_or(false)) => {
                out.extend_from_slice(&fetch(&value_key(key, id))?);
            }
            p => {
                if p.is_none() {
                    skip_value(bytes, &mut pos)?;
                }
                left |= p.is_some();
                out.extend_from_slice(&bytes[at..pos]);
            }
        }
    }
    if !left {
        out[0] = match out[0] {
            ROW_HEADER_EXT => ROW_HEADER,
            ROW_HEADER_TID_EXT => ROW_HEADER_TID,
            h => h,
        };
    }
    Ok(out)
}

/// The pointer of column `col` of a row's encoded values `data` (after its header), when
/// that value is stored out of line.
pub fn pointer_at(data: &[u8], col: usize) -> Option<ValueId> {
    let mut pos = 0;
    let n = get_varint(data, &mut pos).ok()? as usize;
    if col >= n {
        return None;
    }
    for _ in 0..col {
        skip_value(data, &mut pos).ok()?;
    }
    read_pointer(data, &mut pos).ok()?.map(|(id, _)| id)
}

/// Decode the values of a stored row's encoded values `data` (after its header; the row
/// under `key`) that `need` marks into `row`'s cells, the out-of-line ones read through
/// `fetch` and decoded from what it gives: nothing copied but the values decoded.
pub fn decode_into(
    key: &[u8],
    data: &[u8],
    need: &[bool],
    row: &mut [crate::types::Value],
    fetch: &mut dyn FnMut(&[u8]) -> Result<Bytes>,
) -> Result<()> {
    use super::row::decode_value;
    let mut pos = 0;
    let n = get_varint(data, &mut pos)? as usize;
    for i in 0..n.min(row.len()) {
        if !need.get(i).copied().unwrap_or(false) {
            skip_value(data, &mut pos)?;
            continue;
        }
        row[i] = match read_pointer(data, &mut pos)? {
            Some((id, _)) => decode_value(&fetch(&value_key(key, id))?, &mut 0)?,
            None => decode_value(data, &mut pos)?,
        };
    }
    Ok(())
}

/// The pointers of the stored row `bytes` (column, value id).
pub fn pointers(bytes: &[u8]) -> Result<Vec<(usize, ValueId)>> {
    let mut out = Vec::new();
    if !has_external(bytes) {
        return Ok(out);
    }
    let mut pos = header_len(bytes)?;
    let n = get_varint(bytes, &mut pos)? as usize;
    for i in 0..n {
        match read_pointer(bytes, &mut pos)? {
            Some((id, _)) => out.push((i, id)),
            None => skip_value(bytes, &mut pos)?,
        }
    }
    Ok(out)
}

/// Out-of-line value `key` as a reader with snapshot `ts` of `view` sees it.
pub fn read_value(view: &View, key: &[u8], ts: u64) -> Result<Bytes> {
    VALUES_READ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some((_, Some(v))) = view.get_version(key, NEWEST)? {
        return Ok(v);
    }
    match view.get_version(key, ts)? {
        Some((_, Some(v))) => Ok(v),
        _ => Err(internal("an out-of-line value is missing")),
    }
}

/// The stored value `v` of `key` read at `ts` from `view`, with its out-of-line values put
/// back when it is a row that has some.
pub fn resolve(view: &View, key: &[u8], v: Bytes, ts: u64) -> Result<Bytes> {
    if !has_external(&v) || !is_row_key(key) {
        return Ok(v);
    }
    Ok(Bytes::from(splice(key, &v, None, &mut |k| read_value(view, k, ts))?))
}
