//! pg_prewarm (`contrib/pg_prewarm`): load a relation's blocks into the buffer cache. The
//! engine has no shared buffer pool of PostgreSQL pages (pg_buffercache shows every buffer
//! unused), so prewarming reads nothing; what applies is checked as pg_prewarm checks it
//! (the arguments, the relation's storage, SELECT privilege, the fork, the block range) and
//! it reports the blocks of the range, as PostgreSQL's return value counts them.
//! autoprewarm has no buffers to dump or restore.

use super::native::Native;
use super::relinfo;
use crate::Result;
use crate::error::{Error, code};
use crate::exec::sizing::Fork;
use crate::sql::func::FnCtx;
use crate::types::Value;

fn prewarm(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = relinfo::oid_arg(&a[0], "relation")?;
    if a[1].is_null() {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "prewarm type cannot be null"));
    }
    if !matches!(crate::sql::func::text_arg(&a[1]), "prefetch" | "read" | "buffer") {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid prewarm type")
            .hint("Valid prewarm types are \"prefetch\", \"read\", and \"buffer\"."));
    }
    if a[2].is_null() {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "relation fork cannot be null"));
    }
    let fork_name = crate::sql::func::text_arg(&a[2]).to_string();
    let fork = Fork::parse(&fork_name)?;
    let cat = c.exec.catalog();
    let rel = relinfo::open(&cat, oid)?;
    relinfo::check_select(c, &rel)?;
    if !rel.kind.has_storage() {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("relation \"{}\" does not have storage", rel.name))
            .detail(rel.kind.not_supported()));
    }
    if !relinfo::fork_exists(c, oid, fork)? {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("fork \"{fork_name}\" does not exist for this relation"),
        ));
    }
    let nblocks = relinfo::blocks(c, oid, fork)? as i64;
    let bound = |i: usize, which: &str, default: i64| -> Result<i64> {
        match a.get(i).filter(|v| !v.is_null()).and_then(Value::as_i64) {
            None => Ok(default),
            Some(b) if (0..nblocks).contains(&b) => Ok(b),
            Some(_) => Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("{which} block number must be between 0 and {}", nblocks - 1),
            )),
        }
    };
    let first = bound(3, "starting", 0)?;
    let last = bound(4, "ending", nblocks - 1)?;
    Ok(Value::Int8((last - first + 1).max(0)))
}

/// autoprewarm_start_worker: with the library's pg_prewarm.autoprewarm on (its default),
/// the leader worker would restore a dump of buffers: there are none, so nothing runs.
fn start_worker(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Text(std::sync::Arc::from("")))
}

/// autoprewarm_dump_now: the blocks in shared buffers it wrote out (none).
fn dump_now(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int8(0))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(match sym {
        "pg_prewarm" => Native::scalar(prewarm),
        "autoprewarm_start_worker" => Native::scalar(start_worker),
        "autoprewarm_dump_now" => Native::scalar(dump_now),
        _ => return None,
    })
}
