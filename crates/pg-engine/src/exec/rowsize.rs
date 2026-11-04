//! Index entries too big for their pages: the errors PostgreSQL's access methods raise as
//! they form an entry (index_form_tuple's INDEX_SIZE_MASK, `_bt_check_third_page`,
//! GinFormTuple). The engine's entries have no page limit; these checks size each entry as
//! PostgreSQL forms it (`relsize::index_tuple`: its header, alignment and pglz compression of
//! a value over TOAST_INDEX_TARGET) and fail where it fails.
//!
//! Not checked (accepted differences until modelled): a GiST entry's size (its opclass's
//! compressed key: btree_gist's text key fails with "failed to add item to index page" past
//! about 4 kB), a BRIN summary's (its range's min and max values together: a build fails when
//! one passes 8,152 bytes). Hash entries hold a hash code and SP-GiST's text_ops splits long
//! values into prefixes: neither fails.

use super::ExecCtx;
use crate::catalog::Table;
use crate::catalog::relsize::{index_tuple, type_layout};
use crate::error::{Error, Result, code};
use crate::sql::plan::BoundIndex;
use crate::types::{DataType, Value};

/// INDEX_SIZE_MASK: the largest index tuple index_form_tuple makes.
pub const INDEX_SIZE_MASK: usize = 0x1FFF;
/// BTMaxItemSize (btree version 4, 8 kB pages).
pub const BT_MAX_ITEM: usize = 2704;
/// GinMaxItemSize.
const GIN_MAX_ITEM: usize = 2712;
/// pg_opclass of GIN's array_ops.
const GIN_ARRAY_OPS: u32 = 10064;

/// An upper bound on an entry of `vals` under which no limit can be reached (None: size it).
fn surely_small(vals: &[Value]) -> bool {
    let mut n = 16usize;
    for v in vals {
        n += match v {
            Value::Text(s) => s.len() + 8,
            Value::Bytea(b) => b.len() + 8,
            Value::Null | Value::Bool(_) | Value::Int2(_) | Value::Int4(_) | Value::Int8(_) | Value::Float4(_) | Value::Float8(_) => 8,
            Value::Date(_) | Value::Timestamp(_) | Value::Timestamptz(_) | Value::Uuid(_) | Value::Oid(_) => 16,
            _ => return false,
        };
    }
    n < BT_MAX_ITEM
}

pub fn form_error(size: usize) -> Error {
    Error::new(
        code::PROGRAM_LIMIT_EXCEEDED,
        format!("index row requires {size} bytes, maximum size is {INDEX_SIZE_MASK}"),
    )
}

/// A btree entry's tuple size: its key values `vals` and INCLUDE columns' of `row`.
fn btree_size(t: &Table, bix: &BoundIndex, vals: &[Value], row: &[Value], ctx: &ExecCtx) -> Option<usize> {
    let include = bix.index.include.iter().map(|&i| row.get(i).cloned().unwrap_or(Value::Null));
    let all: Vec<Value> = vals.iter().cloned().chain(include).collect();
    if surely_small(&all) {
        return None;
    }
    let mut types: Vec<DataType> = bix.keys.iter().map(|k| k.ty()).collect();
    types.extend(bix.index.include.iter().map(|&i| t.columns[i].ty.clone()));
    let fmt = ctx.fmt();
    let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, fmt);
    Some(index_tuple(&types, &all, &text))
}

/// The checks as an entry of `vals` is formed (index_form_tuple, GinFormTuple), before its
/// index's unique check: `vals` are the index's values of `row` (None: a partial index's
/// predicate left the row out).
pub fn on_form(t: &Table, bix: &BoundIndex, vals: Option<&[Value]>, row: &[Value], ctx: &ExecCtx) -> Result<()> {
    let Some(vals) = vals else { return Ok(()) };
    match bix.index.method.as_str() {
        "btree" => match btree_size(t, bix, vals, row, ctx) {
            Some(n) if n > INDEX_SIZE_MASK => Err(form_error(n)),
            _ => Ok(()),
        },
        "gin" => gin_entries(bix, vals, ctx),
        _ => Ok(()),
    }
}

/// `_bt_check_third_page`, after a btree's unique check: the entry of `vals` (a row stored
/// at `tid`) past BTMaxItemSize.
pub fn on_insert(t: &Table, bix: &BoundIndex, vals: Option<&[Value]>, row: &[Value], tid: impl FnOnce() -> (u32, u16), ctx: &ExecCtx) -> Result<()> {
    let Some(vals) = vals else { return Ok(()) };
    if bix.index.method != "btree" {
        return Ok(());
    }
    match btree_size(t, bix, vals, row, ctx) {
        Some(n) if n > BT_MAX_ITEM => Err(btree_error(t, bix, n, tid(), ctx)),
        _ => Ok(()),
    }
}

/// Whether a btree entry of `vals` is past BTMaxItemSize (its size), for a build's check in
/// index order (`btree_error`).
pub fn btree_oversized(t: &Table, bix: &BoundIndex, vals: &[Value], row: &[Value], ctx: &ExecCtx) -> Result<Option<usize>> {
    match btree_size(t, bix, vals, row, ctx) {
        Some(n) if n > INDEX_SIZE_MASK => Err(form_error(n)),
        Some(n) if n > BT_MAX_ITEM => Ok(Some(n)),
        _ => Ok(None),
    }
}

pub fn btree_error(t: &Table, bix: &BoundIndex, size: usize, (b, o): (u32, u16), ctx: &ExecCtx) -> Error {
    let name = &bix.index.name;
    Error::new(
        code::PROGRAM_LIMIT_EXCEEDED,
        format!("index row size {size} exceeds btree version 4 maximum {BT_MAX_ITEM} for index \"{name}\""),
    )
    .detail(format!("Index row references tuple ({b},{o}) in relation \"{}\".", t.name))
    .hint("Values larger than 1/3 of a buffer page cannot be indexed.\nConsider a function index of an MD5 hash of the value, or use full text indexing.")
    .table(ctx.catalog().schema_name(t.schema), &t.name)
    .constraint(name)
}

/// GinFormTuple of each key array_ops extracts from `vals` (in the keys' order, as
/// ginExtractEntries sorts them), without a posting list (a build's posting tree's case).
fn gin_entries(bix: &BoundIndex, vals: &[Value], ctx: &ExecCtx) -> Result<()> {
    let one = bix.keys.len() == 1;
    for (i, v) in vals.iter().enumerate() {
        let (Value::Array(a), Some(c)) = (v, bix.index.columns.get(i)) else { continue };
        if c.opclass != GIN_ARRAY_OPS || type_layout(&a.elem).0 > 0 {
            continue;
        }
        let mut big: Vec<&Value> = a.items.iter().filter(|e| !e.is_null() && !surely_small(std::slice::from_ref(*e))).collect();
        if big.is_empty() {
            continue;
        }
        big.sort_by(|x, y| x.cmp_total(y));
        let fmt = ctx.fmt();
        let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, fmt);
        for e in big {
            let n = match one {
                true => index_tuple(std::slice::from_ref(&a.elem), std::slice::from_ref(e), &text),
                false => index_tuple(&[DataType::Int2, a.elem.clone()], &[Value::Int2(i as i16 + 1), e.clone()], &text),
            };
            if n > INDEX_SIZE_MASK {
                return Err(form_error(n));
            }
            if n > GIN_MAX_ITEM {
                return Err(Error::new(
                    code::PROGRAM_LIMIT_EXCEEDED,
                    format!("index row size {n} exceeds maximum {GIN_MAX_ITEM} for index \"{}\"", bix.index.name),
                ));
            }
        }
    }
    Ok(())
}
