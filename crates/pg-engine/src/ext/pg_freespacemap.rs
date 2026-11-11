//! pg_freespacemap (`contrib/pg_freespacemap`): the free space map's record of a block, from
//! the engine's size model (`exec::sizing::recorded_free_space`: a heap's simulated FSM, as
//! inserts, deletes, pruning and VACUUM maintain PostgreSQL's).

use super::native::Native;
use super::relinfo;
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::FnCtx;
use crate::types::Value;

fn freespace(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = a[0].as_i64().unwrap_or(0) as u32;
    let blkno = a[1].as_i64().unwrap_or(0);
    let cat = c.exec.catalog();
    let rel = relinfo::open(&cat, oid)?;
    if !rel.kind.has_storage() {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("relation \"{}\" does not have storage", rel.name))
            .detail(rel.kind.not_supported()));
    }
    // (MaxBlockNumber: 0xFFFFFFFE.)
    if !(0..=0xFFFF_FFFE).contains(&blkno) {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid block number"));
    }
    Ok(Value::Int2(crate::exec::sizing::recorded_free_space(oid, blkno as u64, c.exec)? as i16))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    match sym {
        "pg_freespace" => Some(Native::scalar(freespace)),
        _ => None,
    }
}
