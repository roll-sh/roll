//! amcheck (`contrib/amcheck`): verification of relations' integrity. The engine's tables
//! and indexes aren't PostgreSQL's pages, so there are no nbtree/GIN page invariants or
//! heap tuple headers to verify (its storage checksums its own blocks). What applies is
//! checked as amcheck checks it: the target, its access method and validity, the
//! arguments, block ranges against the table's pages. No corruption is reported.

use super::native::Native;
use crate::Result;
use crate::catalog::{Catalog, Index};
use crate::error::{Error, code};
use crate::sql::func::FnCtx;
use crate::types::Value;
use std::sync::Arc;

/// The name of any relation `oid` (for "is not an index").
fn rel_name(cat: &Catalog, oid: u32) -> Option<String> {
    cat.tables
        .get(&oid)
        .map(|t| t.name.clone())
        .or_else(|| cat.views.get(&oid).map(|v| v.name.clone()))
        .or_else(|| cat.sequences.get(&oid).map(|s| s.name.clone()))
        .or_else(|| cat.indexes.get(&oid).map(|i| i.name.clone()))
}

fn open_error(oid: u32) -> Error {
    Error::new(code::UNDEFINED_TABLE, format!("could not open relation with OID {oid}"))
}

/// `amcheck_lock_relation_and_check` / `index_checkable`: the index to verify, of `am`.
fn checkable_index(c: &FnCtx, v: &Value, am: &str) -> Result<Arc<Index>> {
    let oid = v.as_i64().unwrap_or(0) as u32;
    let cat = c.exec.catalog();
    let Some(ix) = cat.indexes.get(&oid).cloned() else {
        return Err(match rel_name(&cat, oid) {
            Some(n) => Error::new(code::WRONG_OBJECT_TYPE, format!("\"{n}\" is not an index")),
            None => open_error(oid),
        });
    };
    // (A partitioned index has no storage: "is not an index" of its kind.)
    if cat.tables.get(&ix.table).is_some_and(|t| t.partkey.is_some()) {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not an index", ix.name)));
    }
    if ix.method != am {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("expected \"{am}\" index as targets for verification"),
        )
        .detail(format!("Relation \"{}\" is a {} index.", ix.name, ix.method)));
    }
    if cat.tables.get(&ix.table).is_some_and(|t| t.temporary)
        && !cat
            .schema_by_name("pg_temp")
            .is_some_and(|s| cat.tables.get(&ix.table).is_some_and(|t| t.schema == s.oid))
    {
        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot access temporary tables of other sessions")
            .detail(format!("Index \"{}\" is associated with temporary relation.", ix.name)));
    }
    if !ix.valid {
        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, format!("cannot check index \"{}\"", ix.name))
            .detail("Index is not valid."));
    }
    Ok(ix)
}

fn void() -> Result<Value> {
    Ok(Value::Text(Arc::from("")))
}

fn bt_index_check(a: &[Value], c: &FnCtx) -> Result<Value> {
    checkable_index(c, &a[0], "btree")?;
    void()
}

fn gin_index_check(a: &[Value], c: &FnCtx) -> Result<Value> {
    checkable_index(c, &a[0], "gin")?;
    void()
}

/// errdetail_relkind_not_supported's wording for verify_heapam.
fn not_supported(cat: &Catalog, oid: u32) -> Option<&'static str> {
    if let Some(t) = cat.tables.get(&oid) {
        if t.partkey.is_some() {
            return Some("This operation is not supported for partitioned tables.");
        }
        if t.foreign.is_some() {
            return Some("This operation is not supported for foreign tables.");
        }
        return None;
    }
    Some(if cat.views.contains_key(&oid) {
        "This operation is not supported for views."
    } else if cat.sequences.contains_key(&oid) {
        // (PostgreSQL 18 checks a sequence's heap.)
        return None;
    } else if cat.indexes.get(&oid).is_some_and(|i| cat.tables.get(&i.table).is_some_and(|t| t.partkey.is_some())) {
        "This operation is not supported for partitioned indexes."
    } else {
        "This operation is not supported for indexes."
    })
}

/// verify_heapam: no corruption rows; the arguments and block range checked as it checks
/// them.
fn verify_heapam(a: &[Value], c: &FnCtx) -> Result<crate::sql::func::SrfIter> {
    let null = |i: usize, what: &str| -> Result<()> {
        if a.get(i).is_none_or(Value::is_null) {
            return Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("{what} cannot be null")));
        }
        Ok(())
    };
    null(0, "relation")?;
    null(1, "on_error_stop")?;
    null(2, "check_toast")?;
    null(3, "skip")?;
    let skip = crate::sql::func::text_arg(&a[3]).to_ascii_lowercase();
    if !matches!(skip.as_str(), "none" | "all-frozen" | "all-visible") {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "invalid skip option")
            .hint("Valid skip options are \"all-visible\", \"all-frozen\", and \"none\"."));
    }
    let oid = a[0].as_i64().unwrap_or(0) as u32;
    let cat = c.exec.catalog();
    let Some(name) = rel_name(&cat, oid) else {
        return Err(open_error(oid));
    };
    if let Some(detail) = not_supported(&cat, oid) {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("cannot check relation \"{name}\"")).detail(detail));
    }
    // (A sequence: its one block.)
    if cat.sequences.contains_key(&oid) {
        return Ok(Box::new(std::iter::empty()));
    }
    let t = cat.tables.get(&oid).cloned().ok_or_else(|| open_error(oid))?;
    let nblocks = crate::exec::sizing::heap_blocks(&t, c.exec)?.unwrap_or(0) as i64;
    let block = |i: usize| a.get(i).filter(|v| !v.is_null()).and_then(Value::as_i64);
    // (An empty table is checked with no block range at all.)
    if nblocks > 0 {
        if let Some(s) = block(4)
            && !(0..nblocks).contains(&s)
        {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("starting block number must be between 0 and {}", nblocks - 1),
            ));
        }
        if let Some(e) = block(5)
            && !(0..nblocks).contains(&e)
        {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("ending block number must be between 0 and {}", nblocks - 1),
            ));
        }
    }
    // (verify_heapam reads through a BAS_BULKREAD strategy: pg_stat_io's bulkread context.
    // A table given new storage in this transaction, as SET TABLESPACE gives it, has none of
    // its pages in shared buffers; otherwise they are all hits.)
    let fresh = c.exec.txn.borrow().storage_new_in_txn(oid);
    crate::exec::pgstat::count_io(c.exec, "relation", "bulkread", if fresh { "reads" } else { "hits" }, nblocks);
    Ok(Box::new(std::iter::empty()))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(match sym {
        "bt_index_check" | "bt_index_parent_check" => Native::scalar(bt_index_check),
        "gin_index_check" => Native::scalar(gin_index_check),
        "verify_heapam" => Native::set(verify_heapam),
        _ => return None,
    })
}
