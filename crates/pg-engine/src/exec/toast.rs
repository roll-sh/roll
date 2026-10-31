//! Rows written with their large values out of line (`storage::toast`): which values go
//! out (by the column's storage and the value's size), an UPDATE keeping an unchanged
//! value's pointer instead of writing it again, and the values a row's last version drops
//! deleted with it.
//!
//! Which values go out is the engine's choice and invisible to clients: what PostgreSQL
//! shows of TOAST (pg_column_size, pg_column_compression, the TOAST relation's size) is the
//! size model's (`sizing`). It follows the column's settings: PLAIN keeps every value in
//! its row, MAIN only a value past TOAST_TUPLE_TARGET_MAIN, EXTENDED and EXTERNAL one past
//! TOAST_TUPLE_THRESHOLD (or the table's toast_tuple_target, when lower).

use crate::catalog::Table;
use crate::error::Result;
use crate::storage::row::{ROW_HEADER, ROW_HEADER_EXT, ROW_HEADER_TID, ROW_HEADER_TID_EXT, encode_value};
use crate::storage::sst::put_varint;
use crate::storage::toast::{self, ValueId};
use crate::txn::Txn;
use crate::types::Value;
use std::sync::atomic::{AtomicU64, Ordering};

/// TOAST_TUPLE_THRESHOLD: values of at least this many encoded bytes go out of line.
const THRESHOLD: usize = 2032;
/// TOAST_TUPLE_TARGET_MAIN: the threshold of a column whose storage is MAIN.
const THRESHOLD_MAIN: usize = 8160;
/// toast_tuple_target's least value: no threshold is lower.
const MIN_THRESHOLD: usize = 128;

/// Out-of-line values' numbers, unique within a transaction (with its xid, a value's id).
static SEQ: AtomicU64 = AtomicU64::new(1);

/// Out-of-line values written (tests read it).
#[doc(hidden)]
pub static VALUES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Per column of `t`, the encoded size from which its values go out of line (None: never).
pub fn thresholds(t: &Table) -> Vec<Option<usize>> {
    let target = super::sizing::toast_tuple_target(t).unwrap_or(THRESHOLD).min(THRESHOLD);
    t.columns
        .iter()
        .map(|c| match c.storage.unwrap_or_else(|| crate::catalog::system::typstorage(&c.ty)) {
            'p' => None,
            'm' => Some(THRESHOLD_MAIN),
            _ => Some(target),
        })
        .collect()
}

/// The encoded size of `v` (`row::encode_value`'s), measured in `scratch` for the kinds
/// whose size isn't at hand; 0 for the fixed-size ones, which never go out of line.
fn encoded_len(v: &Value, scratch: &mut Vec<u8>) -> usize {
    match v {
        Value::Text(s) | Value::Json(s) => s.len() + 5,
        Value::Bytea(b) => b.len() + 5,
        Value::Ext(_, b) => b.len() + 7,
        Value::Null
        | Value::Bool(_)
        | Value::Int2(_)
        | Value::Int4(_)
        | Value::Int8(_)
        | Value::Float4(_)
        | Value::Float8(_)
        | Value::Date(_)
        | Value::Uuid(_)
        | Value::Oid(_)
        | Value::Char(_) => 0,
        _ => {
            scratch.clear();
            encode_value(scratch, v);
            scratch.len()
        }
    }
}

/// Whether any of `row`'s values could have gone out of line when it was written, under
/// any column settings (they change: ALTER TABLE ... SET STORAGE, toast_tuple_target).
fn any_big(row: &[Value], scratch: &mut Vec<u8>) -> bool {
    row.iter().any(|v| encoded_len(v, scratch) >= MIN_THRESHOLD)
}

/// The row stored under `key`: `row` encoded (`scan::encode_table_row_at`'s format) by
/// transaction `xid` as heap TID `tid`, its big values put out of line under keys of their
/// own. `keep`: pointers of the version this one replaces under the same key, with that
/// version's values: a value equal to the one there keeps its pointer (written once).
/// Returns the row's bytes and the kept pointers' ids.
fn encode(
    t: &Table,
    key: &[u8],
    row: &[Value],
    xid: u64,
    tid: Option<(u32, u16)>,
    keep: &[(usize, ValueId)],
    old: Option<&[Value]>,
    txn: &mut Txn,
) -> Result<(Vec<u8>, Vec<ValueId>)> {
    let mut thr: Option<Vec<Option<usize>>> = None;
    let mut out = Vec::with_capacity(32 + row.len() * 10);
    out.push(if tid.is_some() { ROW_HEADER_TID } else { ROW_HEADER });
    put_varint(&mut out, xid);
    if let Some((block, off)) = tid {
        put_varint(&mut out, block as u64);
        put_varint(&mut out, off as u64);
    }
    put_varint(&mut out, row.len() as u64);
    let mut kept = Vec::new();
    let mut ext = false;
    let mut scratch = Vec::new();
    for (i, v) in row.iter().enumerate() {
        let at = out.len();
        encode_value(&mut out, v);
        let len = out.len() - at;
        // (No threshold is below toast_tuple_target's least value.)
        if len < MIN_THRESHOLD
            || thr.get_or_insert_with(|| thresholds(t)).get(i).copied().flatten().is_none_or(|t| len < t)
        {
            continue;
        }
        ext = true;
        let same = keep.iter().find(|(c, _)| *c == i).filter(|_| {
            old.and_then(|o| o.get(i)).is_some_and(|o| {
                scratch.clear();
                encode_value(&mut scratch, o);
                scratch[..] == out[at..]
            })
        });
        let id = match same {
            Some((_, id)) => {
                kept.push(*id);
                *id
            }
            None => {
                let id = ValueId { xid, seq: SEQ.fetch_add(1, Ordering::Relaxed) };
                txn.put(toast::value_key(key, id), out[at..].to_vec())?;
                VALUES_WRITTEN.fetch_add(1, Ordering::Relaxed);
                id
            }
        };
        out.truncate(at);
        toast::put_pointer(&mut out, id, len);
    }
    if ext {
        out[0] = if tid.is_some() { ROW_HEADER_TID_EXT } else { ROW_HEADER_EXT };
    }
    Ok((out, kept))
}

/// Store a new row under `key` (nothing stored there now).
pub fn put_row(t: &Table, key: Vec<u8>, row: &[Value], xid: u64, tid: Option<(u32, u16)>, txn: &mut Txn) -> Result<()> {
    let (bytes, _) = encode(t, &key, row, xid, tid, &[], None, txn)?;
    txn.put(key, bytes)
}

/// `put_row` of a new row under a key never used before (a fresh row id: `Txn::put_fresh`,
/// whose runs are linked into the commit as they are).
pub fn put_new_row(t: &Table, key: Vec<u8>, row: &[Value], xid: u64, tid: Option<(u32, u16)>, txn: &mut Txn) -> Result<()> {
    let (bytes, _) = encode(t, &key, row, xid, tid, &[], None, txn)?;
    txn.put_fresh(key, bytes)
}

/// The pointers of the version stored under `key` now (the one a writer holding its lock
/// replaces), when `old` (its values; None: unknown) may have values out of line.
fn current_pointers(key: &[u8], old: Option<&[Value]>, txn: &Txn) -> Result<Vec<(usize, ValueId)>> {
    if let Some(old) = old
        && !any_big(old, &mut Vec::new())
    {
        return Ok(Vec::new());
    }
    match txn.get_latest_raw(key)? {
        Some(raw) => toast::pointers(&raw),
        None => Ok(Vec::new()),
    }
}

/// Store `row` under `key` in place of the version there now (whose values are `old`): its
/// unchanged out-of-line values kept, the others deleted.
pub fn replace_row(
    t: &Table,
    key: Vec<u8>,
    old: Option<&[Value]>,
    row: &[Value],
    xid: u64,
    tid: Option<(u32, u16)>,
    txn: &mut Txn,
) -> Result<()> {
    let ptrs = current_pointers(&key, old, txn)?;
    let (bytes, kept) = encode(t, &key, row, xid, tid, &ptrs, old, txn)?;
    for (_, id) in ptrs {
        if !kept.contains(&id) {
            txn.delete(toast::value_key(&key, id))?;
        }
    }
    txn.put(key, bytes)
}

/// Delete the row stored under `key` (whose values are `old`; None: unknown) with its
/// out-of-line values.
pub fn delete_row(key: Vec<u8>, old: Option<&[Value]>, txn: &mut Txn) -> Result<()> {
    for (_, id) in current_pointers(&key, old, txn)? {
        txn.delete(toast::value_key(&key, id))?;
    }
    txn.delete(key)
}

/// The end of table `table`'s keys: its rows, index entries and out-of-line values.
pub fn table_end(table: u32) -> Vec<u8> {
    crate::storage::keys::prefix_end(&toast::prefix(table))
}
