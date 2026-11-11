//! Logical replication's functions: pg_get_publication_tables (behind pg_publication_tables)
//! and the subscription statistics functions (no worker ever runs, so they count nothing).

use super::{FnCtx, Registry, SrfIter, text_arg};
use crate::catalog::Catalog;
use crate::catalog::pubsub::{Publication, publishable};
use crate::error::{Error, Result, code};
use crate::types::{DataType, Value};
use std::sync::Arc;

fn int2vector(items: Vec<i16>) -> Value {
    let dims = if items.is_empty() {
        vec![]
    } else {
        vec![(items.len(), 0)]
    };
    Value::Array(Arc::new(crate::types::array::ArrayValue {
        elem: DataType::Int2,
        dims,
        items: items.into_iter().map(Value::Int2).collect(),
    }))
}

/// GetPubPartitionOptionRelations: a partitioned table, or (`leaf`) its leaf partitions.
fn part_option(cat: &Catalog, rel: u32, leaf: bool, out: &mut Vec<u32>) {
    let partitioned = cat.tables.get(&rel).is_some_and(|t| t.partkey.is_some());
    if partitioned && leaf {
        for o in cat.inheritors(rel) {
            if cat.tables.get(&o).is_some_and(|t| t.partkey.is_none()) && !out.contains(&o) {
                out.push(o);
            }
        }
    } else if !out.contains(&rel) {
        out.push(rel);
    }
}

/// The relations a publication publishes, as pg_get_publication_tables lists them.
pub fn publication_relations(cat: &Catalog, p: &Publication) -> Vec<u32> {
    let mut out = Vec::new();
    if p.all_tables {
        // GetAllTablesPublicationRelations.
        for t in cat.tables.values().filter(|t| publishable(t)) {
            let partition = t.partbound.is_some();
            if t.partkey.is_none() && !(partition && p.via_root) {
                out.push(t.oid);
            }
        }
        if p.via_root {
            for t in cat.tables.values().filter(|t| publishable(t)) {
                if t.partkey.is_some() && t.partbound.is_none() {
                    out.push(t.oid);
                }
            }
        }
        return out;
    }
    // GetPublicationRelations, sorted.
    let mut rels = Vec::new();
    for r in &p.rels {
        part_option(cat, r.rel, !p.via_root, &mut rels);
    }
    rels.sort_unstable();
    rels.dedup();
    out.extend(rels);
    // GetAllSchemaPublicationRelations.
    for s in &p.schemas {
        for t in cat.tables.values().filter(|t| t.schema == s.schema && publishable(t)) {
            if t.partkey.is_none() {
                if !out.contains(&t.oid) {
                    out.push(t.oid);
                }
            } else {
                part_option(cat, t.oid, !p.via_root, &mut out);
            }
        }
    }
    out
}

/// pg_get_publication_tables(VARIADIC pubname text[]).
fn publication_tables(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let cat = c.exec.catalog();
    // (The VARIADIC names: an array, or the names themselves when a call lists them.)
    let names: Vec<Value> = a
        .iter()
        .flat_map(|v| match v {
            Value::Array(arr) => arr.items.clone(),
            v => vec![v.clone()],
        })
        .collect();
    let mut infos: Vec<(Arc<Publication>, u32)> = Vec::new();
    let mut via_root = false;
    for n in names.iter().filter(|v| !v.is_null()) {
        let name = text_arg(n);
        let Some(p) = cat.publications.values().find(|p| p.name == name).cloned() else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("publication \"{name}\" does not exist"),
            ));
        };
        via_root |= p.via_root;
        for r in publication_relations(&cat, &p) {
            infos.push((p.clone(), r));
        }
    }
    // filter_partitions: a partition goes when one of its ancestors is listed.
    if via_root {
        let listed: Vec<u32> = infos.iter().map(|i| i.1).collect();
        infos.retain(|(_, r)| {
            !cat.partition_ancestors(*r)
                .iter()
                .any(|anc| listed.contains(anc))
        });
    }
    let mut rows = Vec::new();
    for (p, relid) in infos {
        // Each table is opened (AccessShareLock); one dropped meanwhile is left out.
        let name = cat.tables.get(&relid).map(|t| t.name.clone()).unwrap_or_default();
        c.exec.txn.borrow_mut().lock_relation(
            relid,
            crate::txn::locks::LockMode::AccessShare,
            crate::txn::locks::WaitPolicy::Block,
            &|| name.clone(),
        )?;
        let now = c.exec.txn.borrow().catalog();
        let Some(t) = now.tables.get(&relid).cloned() else { continue };
        let t = &t;
        let in_schema = p.schemas.iter().any(|s| s.schema == t.schema);
        let pr = (!p.all_tables && !in_schema)
            .then(|| p.rels.iter().find(|r| r.rel == relid))
            .flatten();
        let mut attrs = pr.and_then(|r| r.cols.clone()).map(int2vector);
        let qual = pr
            .and_then(|r| r.qual.clone())
            .map_or(Value::Null, |q| Value::Text(Arc::from(q)));
        if attrs.is_none() {
            let cols: Vec<i16> = t
                .columns
                .iter()
                .enumerate()
                .filter(|(_, col)| !col.dropped)
                .filter(|(_, col)| {
                    col.generated.is_none() || (!col.virtual_gen && p.gencols == 's')
                })
                .map(|(i, _)| i as i16 + 1)
                .collect();
            if !cols.is_empty() {
                attrs = Some(int2vector(cols));
            }
        }
        rows.push(vec![
            Value::Oid(p.oid),
            Value::Oid(relid),
            attrs.unwrap_or(Value::Null),
            qual,
        ]);
    }
    Ok(Box::new(rows.into_iter().map(Ok)))
}

/// When each subscription's statistics were last reset (pg_stat_reset_subscription_stats);
/// like Postgres's statistics, not transactional. By cluster too: subscription oids repeat
/// across the engines (and stores) of a process; a cluster is its oid counter, held by a
/// `Weak` so no later counter takes its address.
type Resets = std::collections::HashMap<(usize, u32), (std::sync::Weak<crate::catalog::shared::OidGen>, SubStats)>;

/// A subscription's counters (pg_stat_subscription_stats): apply_error_count,
/// sync_error_count and the conflicts in ConflictType order, and when they were last reset.
#[derive(Default)]
struct SubStats {
    reset: Option<i64>,
    counts: [i64; 9],
}

/// The pg_stat_subscription_stats counters, by column (`SUB_STAT_NAMES`).
pub const SUB_STAT_NAMES: [&str; 9] = [
    "apply_error",
    "sync_error",
    "insert_exists",
    "update_origin_differs",
    "update_exists",
    "update_missing",
    "delete_origin_differs",
    "delete_missing",
    "multiple_unique_conflicts",
];

/// pgstat_report_subscription_error / _conflict: one more of counter `what` (a
/// `SUB_STAT_NAMES` entry) for subscription `subid` of `cat`'s cluster.
pub fn count_subscription_stat(cat: &crate::catalog::Catalog, subid: u32, what: &str) {
    let Some(i) = SUB_STAT_NAMES.iter().position(|n| *n == what) else { return };
    let (k, w) = match &cat.oids {
        Some(g) => (std::sync::Arc::as_ptr(g) as usize, std::sync::Arc::downgrade(g)),
        None => (0, std::sync::Weak::new()),
    };
    let mut r = RESETS.lock();
    r.retain(|_, v| v.0.strong_count() > 0 || v.0.ptr_eq(&std::sync::Weak::new()));
    r.entry((k, subid)).or_insert_with(|| (w, SubStats::default())).1.counts[i] += 1;
}
static RESETS: std::sync::LazyLock<parking_lot::Mutex<Resets>> = std::sync::LazyLock::new(Default::default);

/// The cluster of the running statement (`RESETS`'s key) and a `Weak` holding it.
fn cluster(c: &FnCtx) -> (usize, std::sync::Weak<crate::catalog::shared::OidGen>) {
    match &c.exec.catalog().oids {
        Some(g) => (std::sync::Arc::as_ptr(g) as usize, std::sync::Arc::downgrade(g)),
        None => (0, std::sync::Weak::new()),
    }
}

/// pg_stat_get_subscription_stats(subid): the errors and conflicts counted, when they were
/// last reset.
fn subscription_stats(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let Some(oid) = a[0].as_i64().map(|o| o as u32) else {
        return Ok(Box::new(std::iter::empty()));
    };
    let (k, _) = cluster(c);
    let r = RESETS.lock();
    let st = r.get(&(k, oid)).map(|t| &t.1);
    let mut row = vec![Value::Oid(oid)];
    row.extend((0..9).map(|i| Value::Int8(st.map_or(0, |s| s.counts[i]))));
    row.push(st.and_then(|s| s.reset).map_or(Value::Null, Value::Timestamptz));
    Ok(Box::new(std::iter::once(Ok(row))))
}

/// pg_stat_reset_subscription_stats(subid): its statistics start again now (every
/// subscription's for NULL).
pub fn reset_subscription_stats(a: &[Value], c: &FnCtx) -> Result<Value> {
    let now = crate::types::datetime::Clock::system().now;
    let (k, w) = cluster(c);
    let mut r = RESETS.lock();
    match a[0].as_i64() {
        Some(o) => {
            r.insert((k, o as u32), (w, SubStats { reset: Some(now), counts: [0; 9] }));
        }
        None => {
            for s in c.exec.catalog().subscriptions.keys() {
                r.insert((k, *s), (w.clone(), SubStats { reset: Some(now), counts: [0; 9] }));
            }
        }
    }
    Ok(Value::Null)
}

pub fn register(r: &mut Registry) {
    r.from_pg(6119, None, Some(publication_tables)).variadic();
    r.from_pg(6231, None, Some(subscription_stats));
}
