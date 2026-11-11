//! pg_buffercache (`contrib/pg_buffercache`): the shared buffer pool. The engine has no
//! PostgreSQL buffer pool (its storage caches blocks its own way), so the pool shows as
//! `shared_buffers` buffers none of which holds a relation's page: every row of
//! pg_buffercache is an unused buffer, the summary counts them as unused, and evicting
//! finds nothing to evict. The privilege and argument checks are PostgreSQL's.

use super::native::Native;
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::{FnCtx, SrfIter};
use crate::types::Value;
use std::sync::Arc;

/// NBuffers: the `shared_buffers` setting, in buffers.
fn nbuffers(c: &FnCtx) -> i64 {
    c.exec.sess.settings.borrow().get_raw("shared_buffers").and_then(|v| v.parse().ok()).unwrap_or(16384)
}

/// pg_buffercache_pages: one row per buffer; an unused one is its id and NULLs.
fn pages(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let n = nbuffers(c);
    let width = match c.ret {
        crate::types::DataType::Row(r) => r.fields.len(),
        _ => 9,
    };
    Ok(Box::new((1..=n).map(move |i| {
        let mut row = vec![Value::Null; width.max(1)];
        row[0] = Value::Int4(i as i32);
        Ok(row)
    })))
}

/// pg_buffercache_summary: (used, unused, dirty, pinned, usagecount_avg).
fn summary(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Record(Arc::new(vec![
        Value::Int4(0),
        Value::Int4(nbuffers(c) as i32),
        Value::Int4(0),
        Value::Int4(0),
        Value::Null,
    ])))
}

/// pg_buffercache_usage_counts: per usage count 0 .. BM_MAX_USAGE_COUNT, the buffers.
fn usage_counts(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let n = nbuffers(c) as i32;
    Ok(Box::new((0..=5).map(move |u| {
        Ok(vec![Value::Int4(u), Value::Int4(if u == 0 { n } else { 0 }), Value::Int4(0), Value::Int4(0)])
    })))
}

fn require_superuser(c: &FnCtx, what: &str) -> Result<()> {
    if c.exec.catalog().is_superuser(c.exec.current_user()) {
        return Ok(());
    }
    Err(Error::new(code::INSUFFICIENT_PRIVILEGE, format!("must be superuser to use {what}()")))
}

/// pg_buffercache_evict(buffer): (evicted, flushed); nothing to evict.
fn evict(a: &[Value], c: &FnCtx) -> Result<Value> {
    require_superuser(c, "pg_buffercache_evict")?;
    let b = a[0].as_i64().unwrap_or(0);
    if b < 1 || b > nbuffers(c) {
        return Err(crate::error::internal(format!("bad buffer ID: {b}")));
    }
    Ok(Value::Record(Arc::new(vec![Value::Bool(false), Value::Bool(false)])))
}

/// pg_buffercache_evict_relation(rel): (evicted, flushed, skipped).
fn evict_relation(a: &[Value], c: &FnCtx) -> Result<Value> {
    require_superuser(c, "pg_buffercache_evict_relation")?;
    let oid = a[0].as_i64().unwrap_or(0) as u32;
    let cat = c.exec.catalog();
    if cat.tables.get(&oid).is_some_and(|t| t.temporary) {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "relation uses local buffers, pg_buffercache_evict_relation() is intended to be used for shared buffers only",
        ));
    }
    Ok(Value::Record(Arc::new(vec![Value::Int4(0), Value::Int4(0), Value::Int4(0)])))
}

fn evict_all(_: &[Value], c: &FnCtx) -> Result<Value> {
    require_superuser(c, "pg_buffercache_evict_all")?;
    Ok(Value::Record(Arc::new(vec![Value::Int4(0), Value::Int4(0), Value::Int4(0)])))
}

fn numa_pages(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    Err(crate::error::internal("libnuma initialization failed or NUMA is not supported on this platform"))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(match sym {
        "pg_buffercache_pages" => Native::set(pages),
        "pg_buffercache_numa_pages" => Native::set(numa_pages),
        "pg_buffercache_summary" => Native::scalar(summary),
        "pg_buffercache_usage_counts" => Native::set(usage_counts),
        "pg_buffercache_evict" => Native::scalar(evict),
        "pg_buffercache_evict_relation" => Native::scalar(evict_relation),
        "pg_buffercache_evict_all" => Native::scalar(evict_all),
        _ => return None,
    })
}
