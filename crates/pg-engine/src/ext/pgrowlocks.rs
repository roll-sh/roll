//! pgrowlocks (`contrib/pgrowlocks`): the row locks a table's rows have, from the engine's
//! lock manager (`txn::locks`: the row locks transactions and their subtransactions hold,
//! updates' and deletes' among them) and the rows' TIDs from the heap model. A row locked by
//! more than one (sub)transaction is a multixact, as PostgreSQL's would be; the engine has no
//! MultiXactIds, so its `locker` is the first member's transaction id.

use super::native::Native;
use super::relinfo::{self, Kind};
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::{FnCtx, SrfIter};
use crate::txn::locks::RowLockMode;
use crate::types::{DataType, Value};
use std::sync::Arc;

/// The status pgrowlocks names a lock by.
fn mode_name(mode: RowLockMode, modifies: bool) -> &'static str {
    match (mode, modifies) {
        (RowLockMode::NoKeyUpdate, true) => "No Key Update",
        (RowLockMode::Update, true) => "Update",
        (RowLockMode::KeyShare, _) => "For Key Share",
        (RowLockMode::Share, _) => "For Share",
        (RowLockMode::NoKeyUpdate, _) => "For No Key Update",
        (RowLockMode::Update, _) => "For Update",
    }
}

fn rowlocks(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let names = crate::catalog::names::parse_qualified(crate::sql::func::text_arg(&a[0]))
        .map_err(|_| Error::new(code::INVALID_NAME, "invalid name syntax"))?;
    let (schema, name) = match names.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        [_, s, n] => (Some(s.as_str()), n.as_str()),
        _ => return Err(Error::new(code::SYNTAX_ERROR, "improper relation name (too many dotted names)")),
    };
    let cat = c.exec.catalog();
    let Some(r) = cat.lookup(schema, name, &c.exec.sess.search_path())? else {
        return Err(crate::catalog::no_such_relation(schema, name));
    };
    let rel = relinfo::open(&cat, r.oid())?;
    match rel.kind {
        Kind::PartitionedTable => {
            return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is a partitioned table", rel.name))
                .detail("Partitioned tables do not contain rows."));
        }
        Kind::Table => {}
        _ => return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", rel.name))),
    }
    // SELECT on the table, or pg_stat_scan_tables (role 3377).
    if relinfo::check_select(c, &rel).is_err()
        && !cat.has_privs_of_role(c.exec.current_user(), 3377, c.exec.db_owner())
    {
        relinfo::check_select(c, &rel)?;
    }
    let t = cat.tables.get(&rel.oid).cloned().ok_or_else(|| crate::error::internal("table"))?;
    let start = crate::storage::keys::index_prefix(t.oid, 0);
    let end = crate::storage::keys::prefix_end(&start);
    let rows: Vec<(Vec<u8>, bytes::Bytes)> = c.exec.txn.borrow().scan(&start, &end).collect::<Result<_>>()?;
    let tids = crate::exec::sizing::row_tids(&t, &rows, c.exec);
    let locks = &c.exec.sess.db.locks;
    let mut out = Vec::new();
    for (i, (k, _)) in rows.iter().enumerate() {
        let holders = locks.row_lockers(k);
        if holders.is_empty() {
            continue;
        }
        let tid = tids.as_ref().and_then(|t| t[i]).unwrap_or((0, i as u16 + 1));
        let xid = |x: u64| Value::Oid(x as u32);
        let arr = |ty: DataType, v: Vec<Value>| Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(ty, v)));
        out.push(Ok(vec![
            Value::Tid(tid.0, tid.1),
            xid(holders[0].2),
            Value::Bool(holders.len() > 1),
            arr(DataType::Xid, holders.iter().map(|h| xid(h.2)).collect()),
            arr(DataType::Text, holders.iter().map(|h| Value::text(mode_name(h.0, h.1))).collect()),
            arr(DataType::Int4, holders.iter().map(|h| h.3.map_or(Value::Null, Value::Int4)).collect()),
        ]));
    }
    Ok(Box::new(out.into_iter()))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    match sym {
        "pgrowlocks" => Some(Native::set(rowlocks)),
        _ => None,
    }
}
