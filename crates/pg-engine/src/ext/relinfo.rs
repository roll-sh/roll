//! What the contrib modules that inspect relations (pg_prewarm, pg_freespacemap,
//! pg_visibility, pgstattuple, pgrowlocks) share: opening a relation by oid, its storage and
//! kind as `errdetail_relkind_not_supported` words them, the SELECT privilege check, and its
//! blocks in the engine's size model (`exec::sizing`).

use crate::Result;
use crate::catalog::Catalog;
use crate::error::{Error, code};
use crate::exec::sizing::Fork;
use crate::sql::func::FnCtx;
use crate::types::Value;

/// A relation's kind (relkind), as far as the contrib modules tell them apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Table,
    PartitionedTable,
    Foreign,
    View,
    MatView,
    Index,
    PartitionedIndex,
    Sequence,
    Toast,
    Composite,
}

impl Kind {
    /// RELKIND_HAS_STORAGE.
    pub fn has_storage(self) -> bool {
        matches!(self, Kind::Table | Kind::MatView | Kind::Index | Kind::Sequence | Kind::Toast)
    }

    /// errdetail_relkind_not_supported.
    pub fn not_supported(self) -> &'static str {
        match self {
            Kind::Table => "This operation is not supported for tables.",
            Kind::PartitionedTable => "This operation is not supported for partitioned tables.",
            Kind::Foreign => "This operation is not supported for foreign tables.",
            Kind::View => "This operation is not supported for views.",
            Kind::MatView => "This operation is not supported for materialized views.",
            Kind::Index => "This operation is not supported for indexes.",
            Kind::PartitionedIndex => "This operation is not supported for partitioned indexes.",
            Kind::Sequence => "This operation is not supported for sequences.",
            Kind::Toast => "This operation is not supported for TOAST tables.",
            Kind::Composite => "This operation is not supported for composite types.",
        }
    }
}

/// A relation: its oid, name, kind, and the table it belongs to (an index's, a TOAST
/// table's), for privileges.
pub struct Rel {
    pub oid: u32,
    pub name: String,
    pub kind: Kind,
    pub table: u32,
}

/// The relation with oid `oid` (relation_open), or "could not open relation with OID".
pub fn open(cat: &Catalog, oid: u32) -> Result<Rel> {
    // (relation_open's elog: an internal error.)
    find(cat, oid).ok_or_else(|| Error::new(code::INTERNAL_ERROR, format!("could not open relation with OID {oid}")))
}

pub fn find(cat: &Catalog, oid: u32) -> Option<Rel> {
    let rel = |name: &str, kind, table| Rel { oid, name: name.to_string(), kind, table };
    if let Some(t) = cat.tables.get(&oid) {
        let kind = if t.partkey.is_some() {
            Kind::PartitionedTable
        } else if t.is_foreign() {
            Kind::Foreign
        } else {
            Kind::Table
        };
        return Some(rel(&t.name, kind, oid));
    }
    if let Some(v) = cat.views.get(&oid) {
        return Some(rel(&v.name, if v.materialized { Kind::MatView } else { Kind::View }, oid));
    }
    if let Some(ix) = cat.indexes.get(&oid) {
        let partitioned = cat.tables.get(&ix.table).is_some_and(|t| t.partkey.is_some());
        return Some(rel(&ix.name, if partitioned { Kind::PartitionedIndex } else { Kind::Index }, ix.table));
    }
    if let Some(s) = cat.sequences.get(&oid) {
        return Some(rel(&s.name, Kind::Sequence, oid));
    }
    if let Some((t, index)) = cat.toast_owner(oid) {
        return Some(rel(&t.toast_relname(index), if index { Kind::Index } else { Kind::Toast }, t.oid));
    }
    if let Some(t) = cat.composite_of_rel(oid) {
        return Some(rel(&t.name, Kind::Composite, oid));
    }
    None
}

/// pg_class_aclcheck(..., ACL_SELECT) on the relation (an index's table), with
/// aclcheck_error's message.
pub fn check_select(c: &FnCtx, rel: &Rel) -> Result<()> {
    let cat = c.exec.catalog();
    let held = cat.rel_mask(rel.table, c.exec.current_user(), c.exec.db_owner()).unwrap_or((0, 0));
    if held.0 & crate::catalog::acl::SELECT != 0 {
        return Ok(());
    }
    let (word, _) = crate::sql::binder::perms::rel_word(&cat, rel.oid);
    let word = if rel.kind == Kind::Index { "index" } else { word };
    Err(Error::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied for {word} {}", rel.name)))
}

/// The relation argument `v` (regclass), refusing NULL with `what`.
pub fn oid_arg(v: &Value, what: &str) -> Result<u32> {
    if v.is_null() {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("{what} cannot be null")));
    }
    Ok(v.as_i64().unwrap_or(0) as u32)
}

/// Its blocks in `fork` (the size model's).
pub fn blocks(c: &FnCtx, oid: u32, fork: Fork) -> Result<u64> {
    Ok(crate::exec::sizing::fork_blocks(oid, fork, c.exec)?.unwrap_or(0))
}

/// smgrexists: a relation with storage has its main fork; the others once they have blocks
/// (an unlogged relation's init fork).
pub fn fork_exists(c: &FnCtx, oid: u32, fork: Fork) -> Result<bool> {
    Ok(fork == Fork::Main || blocks(c, oid, fork)? > 0)
}
