//! The row changes of a logical replication apply worker (execReplication.c): a change's row
//! is found through the replica identity index, an index usable with REPLICA IDENTITY FULL or
//! a full-row match, then inserted, updated or deleted through the executor's own row paths
//! (BEFORE / AFTER ROW triggers as session_replication_role allows them, generated columns,
//! checks, unique keys, index entries, the change stream), with no SQL text, plan or command
//! tag of its own. A row is found by its values, never by a remembered row position.

use super::dml::{self, SelfModified};
use super::scan::{Locked, Xltw, decode_table_row, lock_row, lock_row_mode};
use super::trigger::{self, AfterRow};
use super::{ExecCtx, Row};
use crate::catalog::trigger_type as tt;
use crate::catalog::{Catalog, Table};
use crate::error::{Error, Result, code};
use crate::sql::plan::{PartRouting, RowMark, TableRules, Wait};
use crate::storage::keys::{index_prefix, prefix_end};
use crate::txn::locks::RowLockMode;
use crate::types::{DataType, Value};
use std::sync::Arc;

/// How an UPDATE's or DELETE's row is found (FindLogicalRepLocalIndex).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Search {
    /// By the replica identity index or the primary key (its place in `rules.indexes`): the
    /// key alone decides.
    Identity(usize),
    /// By an index usable with the publisher's REPLICA IDENTITY FULL (its place, and how many
    /// leading key columns are plain columns): the whole row decides among its matches.
    Index(usize, usize),
    /// Through the table: the whole row decides.
    Seq,
}

/// A local table changes are applied to (LogicalRepRelMapEntry's local half), bound once per
/// catalog version.
pub struct Target {
    pub rules: TableRules,
    pub route: Option<Arc<PartRouting>>,
    pub search: Search,
    /// The catalog version it was bound at.
    pub version: u64,
}

/// GetRelationIdentityOrPK: the columns of the replica identity index, else of an immediate
/// primary key.
pub fn identity_or_pk(cat: &Catalog, t: &Table) -> Option<Vec<usize>> {
    cat.identity_key(t).or_else(|| {
        t.pkey.clone().filter(|_| {
            !t.constraints
                .iter()
                .any(|c| matches!(c.kind, crate::catalog::ConstraintKind::PrimaryKey { .. }) && c.deferrable)
        })
    })
}

/// How rows of `rules`' table are found, given which of its columns the publisher sends
/// (`sent`) and whether it sends whole old rows (REPLICA IDENTITY FULL).
pub fn search_of(rules: &TableRules, sent: &[bool], remote_full: bool, ctx: &ExecCtx) -> Search {
    let t = &rules.table;
    let cat = ctx.catalog();
    let ident = cat.identity_key(t).is_some() && t.replident == Some('i');
    if identity_or_pk(&cat, t).is_some()
        && let Some(i) = rules.indexes.iter().position(|b| if ident { b.index.replident } else { b.index.primary })
    {
        return Search::Identity(i);
    }
    if !remote_full {
        return Search::Seq;
    }
    // FindUsableIndexForReplicaIdentityFull: the first (by oid) valid, non-partial btree or
    // hash index whose first key is a column the publisher sends.
    let mut usable: Vec<(u32, usize, usize)> = rules
        .indexes
        .iter()
        .enumerate()
        .filter(|(_, b)| b.predicate.is_none() && matches!(b.index.method.as_str(), "btree" | "hash"))
        .filter_map(|(i, b)| {
            let lead = b
                .index
                .columns
                .iter()
                .take_while(|c| matches!(c.key, crate::catalog::IndexKey::Column(k) if sent.get(k).copied().unwrap_or(false)))
                .count();
            // (A hash index takes its one key whole.)
            (lead > 0 && (b.index.method == "btree" || lead == b.index.columns.len())).then_some((b.index.oid, i, lead))
        })
        .collect();
    usable.sort_unstable();
    usable.first().map_or(Search::Seq, |&(_, i, lead)| Search::Index(i, lead))
}

/// Bind `t` for applying changes.
pub fn open(t: &Arc<Table>, sent: &[bool], remote_full: bool, ctx: &ExecCtx) -> Result<Target> {
    let rules = crate::sql::binder::table_rules(t, ctx)?;
    let route = crate::sql::binder::partition_routing(t, ctx)?;
    let search = search_of(&rules, sent, remote_full, ctx);
    Ok(Target { rules, route, search, version: ctx.catalog().version })
}

/// A type tuples_equal can compare (lookup_type_cache's equality operator: a default btree or
/// hash operator class).
fn has_equality(ty: &DataType) -> bool {
    match ty {
        DataType::Json
        | DataType::Xml
        | DataType::Point
        | DataType::Lseg
        | DataType::Path
        | DataType::GBox
        | DataType::Polygon
        | DataType::Line
        | DataType::Circle
        | DataType::PgSnapshot
        | DataType::TxidSnapshot => false,
        DataType::Array(e) => has_equality(e),
        _ => true,
    }
}

/// tuples_equal: whether a local row matches the row searched for in every column the
/// publisher sends (others: NULL in `search`), with the column type's equality.
pub fn tuples_equal(t: &Table, local: &[Value], search: &[Value]) -> Result<bool> {
    for (i, c) in t.columns.iter().enumerate() {
        if c.dropped || c.generated.is_some() {
            continue;
        }
        let (a, b) = (local.get(i).unwrap_or(&Value::Null), search.get(i).unwrap_or(&Value::Null));
        match (a.is_null(), b.is_null()) {
            (true, true) => continue,
            (false, false) => {}
            _ => return Ok(false),
        }
        if !has_equality(&c.ty) {
            return Err(Error::new(
                code::UNDEFINED_FUNCTION,
                format!("could not identify an equality operator for type {}", c.ty.sql_name()),
            ));
        }
        if a.cmp_total(b) != std::cmp::Ordering::Equal {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The storage key of the latest row of `rules`' table matching `search` (the publisher's key
/// or old row over the table's columns, NULL where it sends nothing), without locking it.
fn locate(rules: &TableRules, how: Search, search: &[Value], ctx: &ExecCtx) -> Result<Option<Vec<u8>>> {
    let t = &rules.table;
    let latest_row = |dkey: &[u8]| -> Result<Option<Row>> {
        let got = ctx.txn.borrow().get_latest(dkey)?;
        match got {
            Some((_, Some(b))) => Ok(Some(decode_table_row(t, &b)?)),
            _ => Ok(None),
        }
    };
    match how {
        Search::Identity(i) => {
            let bix = &rules.indexes[i];
            let Some(vals) = dml::index_values(bix, search, ctx)? else { return Ok(None) };
            // (Identity columns are NOT NULL: a NULL key finds nothing.)
            if vals.iter().any(Value::is_null) {
                return Ok(None);
            }
            let nondet = crate::types::collation::index_nondet(&ctx.catalog(), t, &bix.index);
            let nondet = if bix.index.id == 0 { nondet } else { Vec::new() };
            dml::committed_conflict(t, bix, &vals, None, &nondet, ctx)
        }
        Search::Index(i, lead) => {
            let bix = &rules.indexes[i];
            let vals: Vec<Value> = bix.index.columns[..lead]
                .iter()
                .map(|c| match c.key {
                    crate::catalog::IndexKey::Column(k) => search.get(k).cloned().unwrap_or(Value::Null),
                    _ => Value::Null,
                })
                .collect();
            let prefix = super::scan::index_value_prefix(t, &bix.index, &vals, &bix.colls);
            let keys: Vec<Vec<u8>> = {
                let txn = ctx.txn.borrow();
                let mut out = Vec::new();
                for e in txn.scan_latest(&prefix, &prefix_end(&prefix)) {
                    let (_, dkey) = e?;
                    if !out.iter().any(|k: &Vec<u8>| k[..] == dkey[..]) {
                        out.push(dkey.to_vec());
                    }
                }
                out
            };
            for dkey in keys {
                ctx.check()?;
                if let Some(row) = latest_row(&dkey)?
                    && tuples_equal(t, &row, search)?
                {
                    return Ok(Some(dkey));
                }
            }
            Ok(None)
        }
        Search::Seq => {
            let start = index_prefix(t.oid, 0);
            let end = prefix_end(&start);
            let txn = ctx.txn.borrow();
            for e in txn.scan_latest(&start, &end) {
                let (dkey, bytes) = e?;
                if tuples_equal(t, &decode_table_row(t, &bytes)?, search)? {
                    return Ok(Some(dkey.to_vec()));
                }
            }
            Ok(None)
        }
    }
}

/// FindReplTupleInLocalRel and the row's lock (LockTupleExclusive): the row matching
/// `search`, as locked (its key and values); None when there is none. A row changed or
/// deleted while its lock was waited for is looked for again.
pub fn find(rules: &TableRules, how: Search, search: &[Value], delete: bool, ctx: &ExecCtx) -> Result<Option<(Vec<u8>, Row, u64)>> {
    let t = &rules.table;
    loop {
        ctx.check()?;
        let Some(dkey) = locate(rules, how, search, ctx)? else { return Ok(None) };
        let oper = if delete { Xltw::Delete } else { Xltw::Update };
        let (row, xmin) = match lock_row(t, &dkey, RowMark::Update, Wait::Block, oper, ctx)? {
            Locked::Skip => continue,
            Locked::Same => match ctx.txn.borrow().get_latest(&dkey)? {
                Some((_, Some(b))) => (decode_table_row(t, &b)?, super::scan::row_xmin(&b)?),
                _ => continue,
            },
            Locked::Newer(b) => {
                let row = decode_table_row(t, &b)?;
                let still = match how {
                    Search::Identity(i) => {
                        let bix = &rules.indexes[i];
                        dml::index_values(bix, &row, ctx)? == dml::index_values(bix, search, ctx)?
                    }
                    _ => tuples_equal(t, &row, search)?,
                };
                if !still {
                    continue;
                }
                (row, super::scan::row_xmin(&b)?)
            }
        };
        return Ok(Some((dkey, row, xmin)));
    }
}

/// A change's AFTER ROW triggers, at its end (apply's AfterTriggerEndQuery per change; no
/// statement triggers), then its deferrable unique keys' immediate rechecks.
fn fire(rules: &TableRules, mut after: Vec<AfterRow>, event: u16, set_cols: &[usize], ctx: &ExecCtx) -> Result<()> {
    if !after.is_empty() {
        let queue = trigger::take_queue(&rules.triggers, &mut after);
        trigger::batch(ctx, || trigger::fire_after(&rules.triggers, &rules.table, queue, &[event], set_cols, true, false, ctx))?;
    }
    dml::check_statement_rechecks(ctx)
}

/// ExecSimpleRelationInsert: `row` (every column of the target, defaults filled in).
pub fn insert(tg: &Target, row: Row, ctx: &ExecCtx) -> Result<()> {
    let rules = &tg.rules;
    let all: Vec<usize> = (0..row.len()).collect();
    let _target = dml::DmlTarget::push(rules.table.clone(), &rules.table, &[]);
    match &tg.route {
        None => {
            let mut after = Vec::new();
            dml::insert_new(rules, &all, row.clone(), &[], None, &mut after, ctx)
                .map_err(|e| unique_conflict(e, rules, &row, None, "insert_exists", ctx))?;
            fire(rules, after, tt::INSERT, &[], ctx)
        }
        Some(routing) => {
            let mut routed = Vec::new();
            dml::insert_routed(routing, &[], row, &mut routed, ctx)?;
            fire_routed(routing, routed, ctx)
        }
    }
}

fn fire_routed(routing: &PartRouting, routed: Vec<(usize, AfterRow)>, ctx: &ExecCtx) -> Result<()> {
    for (leaf, a) in routed {
        fire(&routing.leaves[leaf].rules, vec![a], tt::INSERT, &[], ctx)?;
    }
    dml::check_statement_rechecks(ctx)
}

/// ExecSimpleRelationUpdate: the found row `old` (key `dkey`) becomes `new`, whose
/// `set_cols` the publisher sent (UPDATE OF triggers).
pub fn update(rules: &TableRules, dkey: &[u8], old: Row, new: Row, set_cols: &[usize], search: &[Value], ctx: &ExecCtx) -> Result<()> {
    let t = &rules.table;
    let _target = dml::DmlTarget::push(t.clone(), t, set_cols);
    let mut new = new;
    if trigger::any(&rules.triggers, true, true, tt::UPDATE, ctx) {
        match trigger::before_row(&rules.triggers, t, tt::UPDATE, Some(&old), new, set_cols, ctx)? {
            Some(r) => new = r,
            None => return Ok(()),
        }
        if let SelfModified::ByLater = dml::self_modified(dkey, ctx) {
            return Err(dml::triggered_change("updated"));
        }
    }
    let keys = super::scan::key_columns(t, &ctx.catalog());
    if keys.iter().any(|&i| old.get(i) != new.get(i)) {
        lock_row_mode(t, dkey, RowLockMode::Update, Wait::Block, Xltw::Update, ctx)?;
    }
    let stored = dml::update_row(rules, dkey, &old, new.clone(), ctx)
        .map_err(|e| unique_conflict(e, rules, &new, Some((dkey, search)), "update_exists", ctx))?;
    let mut after = Vec::new();
    if trigger::wants_after(&rules.triggers, tt::UPDATE, ctx) {
        after.push(AfterRow { event: tt::UPDATE, key: dml::stored_key(t, &stored, dkey), old: Some(old), new: Some(stored) });
    }
    fire(rules, after, tt::UPDATE, set_cols, ctx)
}

/// ExecSimpleRelationDelete: the found row `old` (key `dkey`).
pub fn delete(rules: &TableRules, dkey: &[u8], old: Row, ctx: &ExecCtx) -> Result<()> {
    let t = &rules.table;
    if trigger::any(&rules.triggers, true, true, tt::DELETE, ctx) {
        if !trigger::before_delete(&rules.triggers, t, &old, ctx)? {
            return Ok(());
        }
        if let SelfModified::ByLater = dml::self_modified(dkey, ctx) {
            return Err(dml::triggered_change("deleted"));
        }
    }
    dml::delete_row(t, &rules.indexes, dkey, &old, ctx)?;
    let mut after = Vec::new();
    if trigger::wants_after(&rules.triggers, tt::DELETE, ctx) {
        after.push(AfterRow { event: tt::DELETE, old: Some(old), ..Default::default() });
    }
    fire(rules, after, tt::DELETE, &[], ctx)
}

/// "conflict detected on relation ..." (ReportApplyConflict): its message.
pub fn conflict_message(t: &Table, kind: &str, ctx: &ExecCtx) -> String {
    format!("conflict detected on relation \"{}.{}\": conflict={kind}", ctx.catalog().schema_name(t.schema), t.name)
}

/// GetTupleTransactionInfo: the commit timestamp and origin of a local row's xmin, when
/// track_commit_timestamp is on and has them.
pub fn row_commit(xmin: u64, ctx: &ExecCtx) -> Option<crate::repl::commit_ts::Stamp> {
    let db = ctx.txn.borrow().db.clone();
    if !db.log.commit_ts.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    crate::repl::commit_ts::get(&db, xmin).ok().flatten()
}

/// errdetail_apply_conflict's "modified locally / by origin \"x\" / by a non-existent origin
/// in transaction N at <time>" (`what` before it, as the conflict words it).
pub fn modified_by(what: &str, by_origin: &str, xmin: u64, s: crate::repl::commit_ts::Stamp, ctx: &ExecCtx) -> String {
    let at = crate::types::text::format_value(&Value::Timestamptz(s.ts), &DataType::Timestamptz(-1), ctx.fmt());
    let xmin = xmin as u32;
    if s.origin == 0 {
        return format!("{what} modified locally in transaction {xmin} at {at}.");
    }
    let name = ctx.txn.borrow().db.repl().and_then(|r| r.origins.name_of(s.origin));
    match name {
        Some(n) => format!("{what} modified by {by_origin}\"{n}\" in transaction {xmin} at {at}."),
        None => format!("{what} modified by a non-existent origin in transaction {xmin} at {at}."),
    }
}

/// BuildIndexValueDescription: "(a)=(1)", when the user may see the key.
fn index_desc(t: &Table, bix: &crate::sql::plan::BoundIndex, row: &[Value], ctx: &ExecCtx) -> Option<String> {
    let vals = dml::index_values(bix, row, ctx).ok()??;
    dml::key_visible(t, bix, ctx).then(|| dml::key_detail(t, bix, &vals, ctx, "").trim_start_matches("Key ").trim_end().to_string())
}

/// ExecBuildSlotValueDescription: "(1, abc)".
fn row_desc(t: &Table, row: &[Value], ctx: &ExecCtx) -> Option<String> {
    dml::failing_row(t, row, ctx).map(|d| d.trim_start_matches("Failing row contains ").trim_end_matches('.').to_string())
}

/// The replica identity index or primary key, if the table has one.
fn ident_index<'a>(rules: &'a TableRules, ctx: &ExecCtx) -> Option<&'a crate::sql::plan::BoundIndex> {
    let t = &rules.table;
    let cat = ctx.catalog();
    identity_or_pk(&cat, t)?;
    let by_ident = cat.identity_key(t).is_some() && t.replident == Some('i');
    rules.indexes.iter().find(|b| if by_ident { b.index.replident } else { b.index.primary })
}

/// build_tuple_value_details: "Key ...; existing local row ...; remote row ...; replica
/// identity ...", each part capitalized when it comes first.
pub fn value_details(
    rules: &TableRules,
    key: Option<(&crate::sql::plan::BoundIndex, &[Value])>,
    local: Option<&[Value]>,
    remote: Option<&[Value]>,
    search: Option<&[Value]>,
    ctx: &ExecCtx,
) -> Option<String> {
    let t = &rules.table;
    let mut parts: Vec<String> = Vec::new();
    let mut add = |label: &str, d: Option<String>| {
        if let Some(d) = d {
            let label = if parts.is_empty() {
                let mut c = label.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            } else {
                label.to_string()
            };
            parts.push(format!("{label} {d}"));
        }
    };
    if let Some((bix, row)) = key {
        add("key", index_desc(t, bix, row, ctx));
    }
    if let Some(l) = local {
        add("existing local row", row_desc(t, l, ctx));
    }
    if let Some(r) = remote {
        add("remote row", row_desc(t, r, ctx));
    }
    if let Some(s) = search {
        match ident_index(rules, ctx) {
            Some(bix) => add("replica identity", index_desc(t, bix, s, ctx)),
            None => add("replica identity full", row_desc(t, s, ctx)),
        }
    }
    (!parts.is_empty()).then(|| format!("{}.", parts.join("; ")))
}

/// A unique violation of an applied row as the conflict it is (insert_exists, update_exists,
/// multiple_unique_conflicts): each immediate unique index the row collides on, with the
/// existing row and the transaction that wrote it. Other errors pass.
fn unique_conflict(e: Error, rules: &TableRules, row: &[Value], own: Option<(&[u8], &[Value])>, kind: &str, ctx: &ExecCtx) -> Error {
    if e.code != code::UNIQUE_VIOLATION {
        return e;
    }
    let t = &rules.table;
    // (The remote row as it would have been stored: its generated columns computed.)
    let mut row = row.to_vec();
    for (i, g) in rules.generated.iter().enumerate() {
        if let Some(g) = g
            && let Ok(v) = crate::sql::expr::eval(g, &row, ctx)
        {
            row[i] = v;
        }
    }
    let row = &row[..];
    let mut details = Vec::new();
    for bix in rules.indexes.iter().filter(|b| b.index.unique && b.constraint.as_ref().is_none_or(|c| !c.deferral.0)) {
        let Ok(Some(vals)) = dml::index_values(bix, row, ctx) else { continue };
        let Ok(Some(dkey)) = dml::committed_conflict(t, bix, &vals, own.map(|o| o.0), &[], ctx) else { continue };
        let Ok(Some((_, Some(bytes)))) = ctx.txn.borrow().get_latest(&dkey) else { continue };
        let xmin = super::scan::row_xmin(&bytes).unwrap_or(0);
        let local = decode_table_row(t, &bytes).ok();
        let what = format!("Key already exists in unique index \"{}\",", bix.index.name);
        let mut d = match row_commit(xmin, ctx) {
            Some(s) => modified_by(&what, "origin ", xmin, s, ctx),
            None => format!("{what} modified in transaction {}.", xmin as u32),
        };
        if let Some(v) = value_details(rules, Some((bix, row)), local.as_deref(), Some(row), own.map(|o| o.1), ctx) {
            d.push('\n');
            d.push_str(&v);
        }
        details.push(d);
    }
    if details.is_empty() {
        return e;
    }
    let kind = if details.len() > 1 { "multiple_unique_conflicts" } else { kind };
    Error::new(code::UNIQUE_VIOLATION, conflict_message(t, kind, ctx)).detail(details.join("\n"))
}
