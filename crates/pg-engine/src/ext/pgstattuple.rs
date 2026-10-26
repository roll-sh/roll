//! pgstattuple (`contrib/pgstattuple`): tuple- and page-level statistics, from the engine's
//! size model (`exec::sizing`): a heap's pages and tuple versions (`heapsim`), a btree's
//! pages (`btsim`). The relations, kinds and errors are PostgreSQL's; byte counts follow
//! the model, an accepted internal difference where the model and PostgreSQL's pages part.
//! The GIN pending list and hash index pages aren't modelled page by page: their counts are
//! the shape an index of that size has (its pages beyond the meta, bitmap and buckets).

use super::native::Native;
use super::relinfo::{self, Kind, Rel};
use crate::Result;
use crate::error::{Error, code};
use crate::exec::sizing::{self, Fork};
use crate::sql::func::FnCtx;
use crate::types::Value;
use std::sync::Arc;

const BLCKSZ: i64 = 8192;

fn superuser(c: &FnCtx) -> Result<()> {
    if c.exec.catalog().is_superuser(c.exec.current_user()) {
        return Ok(());
    }
    Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "must be superuser to use pgstattuple functions"))
}

/// relation_openrv over textToQualifiedNameList.
fn by_name(c: &FnCtx, v: &Value) -> Result<Rel> {
    let names = crate::catalog::names::parse_qualified(crate::sql::func::text_arg(v))
        .map_err(|_| Error::new(code::INVALID_NAME, "invalid name syntax"))?;
    let (schema, name) = match names.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        [_, s, n] => (Some(s.as_str()), n.as_str()),
        _ => return Err(Error::new(code::SYNTAX_ERROR, "improper relation name (too many dotted names)")),
    };
    let cat = c.exec.catalog();
    match cat.lookup(schema, name, &c.exec.sess.search_path())? {
        Some(r) => relinfo::open(&cat, r.oid()),
        None => Err(crate::catalog::no_such_relation(schema, name)),
    }
}

fn by_oid(c: &FnCtx, v: &Value) -> Result<Rel> {
    relinfo::open(&c.exec.catalog(), v.as_i64().unwrap_or(0) as u32)
}

/// A percentage as pgstattuple prints it ("%.2f").
fn pct(part: u64, whole: i64) -> Value {
    let p = if whole == 0 { 0.0 } else { 100.0 * part as f64 / whole as f64 };
    Value::Float8((p * 100.0).round() / 100.0)
}

/// build_pgstattuple_type.
fn tuple_row(table_len: i64, live: u64, live_len: u64, dead: u64, dead_len: u64, free: u64) -> Value {
    Value::Record(Arc::new(vec![
        Value::Int8(table_len),
        Value::Int8(live as i64),
        Value::Int8(live_len as i64),
        pct(live_len, table_len),
        Value::Int8(dead as i64),
        Value::Int8(dead_len as i64),
        pct(dead_len, table_len),
        Value::Int8(free as i64),
        pct(free, table_len),
    ]))
}

fn index_of(c: &FnCtx, rel: &Rel) -> Option<Arc<crate::catalog::Index>> {
    c.exec.catalog().indexes.get(&rel.oid).cloned()
}

fn valid(rel: &Rel, ix: &crate::catalog::Index) -> Result<()> {
    if !ix.valid {
        return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("index \"{}\" is not valid", rel.name)));
    }
    Ok(())
}

/// pgstat_relation.
fn stat_relation(c: &FnCtx, rel: Rel) -> Result<Value> {
    let blocks = relinfo::blocks(c, rel.oid, Fork::Main)? as i64;
    match rel.kind {
        Kind::Table | Kind::MatView | Kind::Toast => {
            let (live, ll, dead, dl, free) = sizing::heap_tuple_stats(rel.oid, c.exec)?.unwrap_or_default();
            Ok(tuple_row(blocks * BLCKSZ, live, ll, dead, dl, free))
        }
        // (A sequence: its one tuple.)
        Kind::Sequence => Ok(tuple_row(BLCKSZ, 1, 41, 0, 0, 8104)),
        Kind::Index => {
            let ix = index_of(c, &rel).ok_or_else(|| crate::error::internal("index"))?;
            valid(&rel, &ix)?;
            match ix.method.as_str() {
                "btree" => {
                    let (s, _, _) = sizing::btree_page_stats(rel.oid, c.exec)?.unwrap_or_default();
                    Ok(tuple_row(blocks * BLCKSZ, s.items, s.item_bytes, 0, 0, s.free))
                }
                "hash" | "gist" => {
                    let free = (blocks.max(1) - 1) as u64 * (BLCKSZ as u64 - 40);
                    Ok(tuple_row(blocks * BLCKSZ, 0, 0, 0, 0, free))
                }
                m => Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("index \"{}\" ({} index) is not supported", rel.name, m),
                )),
            }
        }
        k => Err(Error::new(code::FEATURE_NOT_SUPPORTED, format!("cannot get tuple-level statistics for relation \"{}\"", rel.name))
            .detail(k.not_supported())),
    }
}

fn st_name(a: &[Value], c: &FnCtx) -> Result<Value> {
    superuser(c)?;
    stat_relation(c, by_name(c, &a[0])?)
}
fn st_name15(a: &[Value], c: &FnCtx) -> Result<Value> {
    stat_relation(c, by_name(c, &a[0])?)
}
fn st_oid(a: &[Value], c: &FnCtx) -> Result<Value> {
    superuser(c)?;
    stat_relation(c, by_oid(c, &a[0])?)
}
fn st_oid15(a: &[Value], c: &FnCtx) -> Result<Value> {
    stat_relation(c, by_oid(c, &a[0])?)
}

/// pgstatindex_impl.
fn stat_index(c: &FnCtx, rel: Rel) -> Result<Value> {
    let ix = index_of(c, &rel).filter(|i| rel.kind == Kind::Index && i.method == "btree");
    let Some(ix) = ix else {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("relation \"{}\" is not a btree index", rel.name)));
    };
    valid(&rel, &ix)?;
    let (s, level, blocks) = sizing::btree_page_stats(rel.oid, c.exec)?
        .unwrap_or((Default::default(), 0, relinfo::blocks(c, rel.oid, Fork::Main)? as u32));
    let round2 = |x: f64| if x.is_nan() { x } else { (x * 100.0).round() / 100.0 };
    let density = if s.leaf == 0 {
        f64::NAN
    } else {
        let max_avail = (s.leaf * crate::catalog::btsim::BT_PAGE_SPACE as u64) as f64;
        100.0 - s.leaf_free as f64 / max_avail * 100.0
    };
    let fragmentation = if s.leaf == 0 { f64::NAN } else { s.fragments as f64 * 100.0 / s.leaf as f64 };
    Ok(Value::Record(Arc::new(vec![
        Value::Int4(4),
        Value::Int4(level as i32),
        Value::Int8(blocks as i64 * BLCKSZ),
        Value::Int8(s.root as i64),
        Value::Int8(s.internal as i64),
        Value::Int8(s.leaf as i64),
        Value::Int8(s.empty as i64),
        Value::Int8(0),
        Value::Float8(round2(density)),
        Value::Float8(round2(fragmentation)),
    ])))
}

fn ix_name(a: &[Value], c: &FnCtx) -> Result<Value> {
    superuser(c)?;
    stat_index(c, by_name(c, &a[0])?)
}
fn ix_name15(a: &[Value], c: &FnCtx) -> Result<Value> {
    stat_index(c, by_name(c, &a[0])?)
}
fn ix_oid(a: &[Value], c: &FnCtx) -> Result<Value> {
    superuser(c)?;
    stat_index(c, by_oid(c, &a[0])?)
}
fn ix_oid15(a: &[Value], c: &FnCtx) -> Result<Value> {
    stat_index(c, by_oid(c, &a[0])?)
}

/// pg_relpages_impl.
fn relpages(c: &FnCtx, rel: Rel) -> Result<Value> {
    if !rel.kind.has_storage() {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("cannot get page count of relation \"{}\"", rel.name))
            .detail(rel.kind.not_supported()));
    }
    Ok(Value::Int8(relinfo::blocks(c, rel.oid, Fork::Main)? as i64))
}

fn rp_name(a: &[Value], c: &FnCtx) -> Result<Value> {
    superuser(c)?;
    relpages(c, by_name(c, &a[0])?)
}
fn rp_name15(a: &[Value], c: &FnCtx) -> Result<Value> {
    relpages(c, by_name(c, &a[0])?)
}
fn rp_oid(a: &[Value], c: &FnCtx) -> Result<Value> {
    superuser(c)?;
    relpages(c, by_oid(c, &a[0])?)
}
fn rp_oid15(a: &[Value], c: &FnCtx) -> Result<Value> {
    relpages(c, by_oid(c, &a[0])?)
}

/// pgstatginindex_internal: (version, pending pages, pending tuples).
fn gin(c: &FnCtx, rel: Rel) -> Result<Value> {
    let ix = index_of(c, &rel).filter(|i| rel.kind == Kind::Index && i.method == "gin");
    let Some(ix) = ix else {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("relation \"{}\" is not a GIN index", rel.name)));
    };
    valid(&rel, &ix)?;
    Ok(Value::Record(Arc::new(vec![Value::Int4(2), Value::Int4(0), Value::Int8(0)])))
}

fn gin_su(a: &[Value], c: &FnCtx) -> Result<Value> {
    superuser(c)?;
    gin(c, by_oid(c, &a[0])?)
}
fn gin15(a: &[Value], c: &FnCtx) -> Result<Value> {
    gin(c, by_oid(c, &a[0])?)
}

/// pgstathashindex: (version, bucket pages, overflow pages, bitmap pages, unused pages, live
/// items, dead items, free percent).
fn hash(a: &[Value], c: &FnCtx) -> Result<Value> {
    let rel = by_oid(c, &a[0])?;
    let ix = index_of(c, &rel).filter(|i| rel.kind == Kind::Index && i.method == "hash");
    let Some(ix) = ix else {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("relation \"{}\" is not a hash index", rel.name)));
    };
    valid(&rel, &ix)?;
    let blocks = relinfo::blocks(c, rel.oid, Fork::Main)? as i64;
    // (The metapage, one bitmap page, the rest buckets.)
    let buckets = (blocks - 2).max(0);
    let live = c.exec.catalog().relstats.get(&rel.oid).map_or(0, |s| s.tuples.max(0.0) as i64);
    let space = buckets as f64 * (BLCKSZ - 40) as f64;
    let used = live as f64 * 20.0;
    let free = if space == 0.0 { 0.0 } else { 100.0 * (space - used).max(0.0) / space };
    Ok(Value::Record(Arc::new(vec![
        Value::Int4(4),
        Value::Int8(buckets),
        Value::Int8(0),
        Value::Int8(1),
        Value::Int8(0),
        Value::Int8(live),
        Value::Int8(0),
        Value::Float8(free),
    ])))
}

/// pgstattuple_approx_internal: heaps only; the counts the model has exactly.
fn approx(c: &FnCtx, rel: Rel) -> Result<Value> {
    if !matches!(rel.kind, Kind::Table | Kind::MatView | Kind::Toast) {
        return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("relation \"{}\" is of wrong relation kind", rel.name))
            .detail(rel.kind.not_supported()));
    }
    let blocks = relinfo::blocks(c, rel.oid, Fork::Main)? as i64;
    let (live, ll, dead, dl, free) = sizing::heap_tuple_stats(rel.oid, c.exec)?.unwrap_or_default();
    let len = blocks * BLCKSZ;
    Ok(Value::Record(Arc::new(vec![
        Value::Int8(len),
        Value::Float8(if blocks == 0 { 0.0 } else { 100.0 }),
        Value::Int8(live as i64),
        Value::Int8(ll as i64),
        pct(ll, len),
        Value::Int8(dead as i64),
        Value::Int8(dl as i64),
        pct(dl, len),
        Value::Int8(free as i64),
        pct(free, len),
    ])))
}

fn approx_su(a: &[Value], c: &FnCtx) -> Result<Value> {
    superuser(c)?;
    approx(c, by_oid(c, &a[0])?)
}
fn approx15(a: &[Value], c: &FnCtx) -> Result<Value> {
    approx(c, by_oid(c, &a[0])?)
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    Some(Native::scalar(match sym {
        "pgstattuple" => st_name,
        "pgstattuple_v1_5" => st_name15,
        "pgstattuplebyid" => st_oid,
        "pgstattuplebyid_v1_5" => st_oid15,
        "pgstatindex" => ix_name,
        "pgstatindex_v1_5" => ix_name15,
        "pgstatindexbyid" => ix_oid,
        "pgstatindexbyid_v1_5" => ix_oid15,
        "pg_relpages" => rp_name,
        "pg_relpages_v1_5" => rp_name15,
        "pg_relpagesbyid" => rp_oid,
        "pg_relpagesbyid_v1_5" => rp_oid15,
        "pgstatginindex" => gin_su,
        "pgstatginindex_v1_5" => gin15,
        "pgstathashindex" => hash,
        "pgstattuple_approx" => approx_su,
        "pgstattuple_approx_v1_5" => approx15,
        _ => return None,
    }))
}
