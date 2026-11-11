//! pg_visibility (`contrib/pg_visibility`): a table's visibility map, from the engine's heap
//! model (`exec::sizing::visibility`), which sets pages all-visible as VACUUM and COPY FREEZE
//! do and clears them as writes do. A page is all-frozen when all-visible with every tuple
//! frozen. pg_check_frozen / pg_check_visible look for tuples a marked page has that aren't
//! frozen / visible to all: the model marks a page only when none are, so they find none.

use super::native::Native;
use super::relinfo::{self, Kind};
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::{FnCtx, SrfIter};
use crate::types::Value;
use std::sync::Arc;

/// relation_open and check_relation_relkind (RELKIND_HAS_TABLE_AM): the map, per block.
fn map_of(c: &FnCtx, v: &Value) -> Result<Vec<(bool, bool, bool)>> {
    let oid = v.as_i64().unwrap_or(0) as u32;
    let cat = c.exec.catalog();
    let rel = relinfo::open(&cat, oid)?;
    if !matches!(rel.kind, Kind::Table | Kind::MatView | Kind::Toast) {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("relation \"{}\" is of wrong relation kind", rel.name))
            .detail(rel.kind.not_supported()));
    }
    Ok(crate::exec::sizing::visibility(oid, c.exec)?.unwrap_or_default())
}

fn block(v: &Value) -> Result<i64> {
    let b = v.as_i64().unwrap_or(0);
    if !(0..=0xFFFF_FFFE).contains(&b) {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid block number"));
    }
    Ok(b)
}

/// pg_visibility_map(rel, blkno): (all_visible, all_frozen); a block past the end has none.
fn map_block(a: &[Value], c: &FnCtx) -> Result<Value> {
    let map = map_of(c, &a[0])?;
    let b = block(&a[1])?;
    let (v, f, _) = map.get(b as usize).copied().unwrap_or_default();
    Ok(Value::Record(Arc::new(vec![Value::Bool(v), Value::Bool(f)])))
}

/// pg_visibility(rel, blkno): the map's bits and PD_ALL_VISIBLE (false past the end).
fn vis_block(a: &[Value], c: &FnCtx) -> Result<Value> {
    let map = map_of(c, &a[0])?;
    let b = block(&a[1])?;
    let (v, f, pd) = map.get(b as usize).copied().unwrap_or_default();
    Ok(Value::Record(Arc::new(vec![Value::Bool(v), Value::Bool(f), Value::Bool(pd)])))
}

fn map_rel(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let map = map_of(c, &a[0])?;
    Ok(Box::new(map.into_iter().enumerate().map(|(i, (v, f, _))| {
        Ok(vec![Value::Int8(i as i64), Value::Bool(v), Value::Bool(f)])
    })))
}

fn vis_rel(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let map = map_of(c, &a[0])?;
    Ok(Box::new(map.into_iter().enumerate().map(|(i, (v, f, pd))| {
        Ok(vec![Value::Int8(i as i64), Value::Bool(v), Value::Bool(f), Value::Bool(pd)])
    })))
}

/// pg_visibility_map_summary: (all_visible, all_frozen) pages.
fn summary(a: &[Value], c: &FnCtx) -> Result<Value> {
    let map = map_of(c, &a[0])?;
    let visible = map.iter().filter(|m| m.0).count() as i64;
    let frozen = map.iter().filter(|m| m.1).count() as i64;
    Ok(Value::Record(Arc::new(vec![Value::Int8(visible), Value::Int8(frozen)])))
}

/// pg_check_frozen / pg_check_visible: the tuples breaking what the map claims (none).
fn check(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    map_of(c, &a[0])?;
    Ok(Box::new(std::iter::empty()))
}

fn truncate(a: &[Value], c: &FnCtx) -> Result<Value> {
    map_of(c, &a[0])?;
    crate::exec::sizing::truncate_vm(a[0].as_i64().unwrap_or(0) as u32, c.exec)?;
    Ok(Value::Text(Arc::from("")))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(match sym {
        "pg_visibility_map" => Native::scalar(map_block),
        "pg_visibility" => Native::scalar(vis_block),
        "pg_visibility_map_rel" => Native::set(map_rel),
        "pg_visibility_rel" => Native::set(vis_rel),
        "pg_visibility_map_summary" => Native::scalar(summary),
        "pg_check_frozen" | "pg_check_visible" => Native::set(check),
        "pg_truncate_visibility_map" => Native::scalar(truncate),
        _ => return None,
    })
}
