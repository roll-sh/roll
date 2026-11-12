//! Items as rows of the base table (and of index views), read and written through the engine.
//!
//! Writes, as ScyllaDB's Alternator makes them (verified through the CDC log of the reference,
//! whose operation column shows what a write consists of):
//!
//! - **PutItem**: a row marker, a tombstone of the whole `:attrs` map (at `ts - 1`, as a CQL
//!   collection overwrite) and one element per non-key attribute. CDC logs it as an insert.
//! - **UpdateItem**: elements (or element tombstones) of the attributes it touched, and a row
//!   marker when it sets something (an item created by an update exists until deleted, even
//!   with no attribute left). CDC logs it as an insert when it has a marker, else an update.
//! - A write that read the item (a condition, `ReturnValues`, an update reading old values)
//!   or that the stream post-images, and finds the item, has no row marker: CDC logs it as an
//!   update (MODIFY in KEYS_ONLY streams).
//! - **DeleteItem**: a row deletion, or a partition deletion when the table has no range key.
//! - With post-images (NEW_IMAGE, NEW_AND_OLD_IMAGES), a write that changes nothing is not
//!   made at all, so the stream has no record of it.
//!
//! Every write is a read-modify-write under the partition's LWT lock (`Database::lock_partition`,
//! which CQL `IF` statements take too), and is applied with the engine's write observers, so
//! views and the CDC log follow atomically.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use nosql_engine::Database;
use nosql_engine::schema::Table;
use nosql_storage::compare::compare_bounds;
use nosql_storage::read::{ClusteringFilter, Limits, PartitionRead, RangeRead, Resume};
use nosql_storage::sstable::cursor::KeyBound;
use nosql_storage::{
    Bound, Cell, Clustering, ColumnData, DeletionTime, LivenessInfo, PartitionKey, PartitionUpdate,
    Row, RowDeletion, Slice,
};
use nosql_types::Dialect;

use super::catalog::{ATTRS, scalar};
use super::engine_error;
use crate::backend::*;
use crate::error::{Error, Result};
use crate::json::Json;
use crate::schema::TableSchema;
use crate::storage::{decode_key, decode_value, encode_key, encode_value};
use crate::value::Item;

pub(crate) fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn internal(e: impl std::fmt::Display) -> Error {
    Error::internal(e.to_string())
}

/// The CQL serialisation of a key value.
fn key_bytes(v: &Json) -> Result<Bytes> {
    encode_key(v)
        .map(Bytes::from)
        .ok_or_else(|| Error::internal("invalid key value"))
}

pub(crate) fn partition_key(v: &Json) -> Result<PartitionKey> {
    nosql_engine::storage::partition_key(&[key_bytes(v)?], Dialect::Scylla, true)
        .map_err(|e| Error::validation(e.message))
}

/// An item from a stored row of a table or view: its key columns and `:attrs`.
pub fn row_item(t: &Table, pk: &[u8], row: &Row) -> Item {
    let mut item = Item::new();
    if let Some(c) = t.column(ATTRS)
        && let Some(ColumnData::Complex { cells, .. }) = row.column(c.id)
    {
        for cell in cells.iter().filter(|c| !c.is_tombstone()) {
            let Some(name) = cell
                .path
                .as_deref()
                .and_then(|p| std::str::from_utf8(p).ok())
            else {
                continue;
            };
            if let Some(v) = decode_value(&cell.value) {
                item.insert(name.to_string(), v);
            }
        }
    }
    let parts = nosql_engine::storage::key_components(t, pk);
    for (c, v) in t.partition_key_columns().zip(parts) {
        if let (Some(ty), Some(v)) = (scalar(&c.ty), v)
            && let Some(j) = decode_key(ty, &v)
        {
            item.insert(c.name.clone(), j);
        }
    }
    for (i, c) in t.clustering_columns().enumerate() {
        if let (Some(ty), Some(v)) = (scalar(&c.ty), row.clustering.get(i))
            && let Some(j) = decode_key(ty, &v)
        {
            item.insert(c.name.clone(), j);
        }
    }
    item
}

/// The stored size of a row, for page byte limits (as the in-memory backend counts it).
fn row_size(t: &Table, pk: &[u8], row: &Row) -> usize {
    let mut n = pk.len() + row.clustering.components().map(|c| c.len()).sum::<usize>();
    if let Some(c) = t.column(ATTRS)
        && let Some(ColumnData::Complex { cells, .. }) = row.column(c.id)
    {
        n += cells
            .iter()
            .map(|c| c.path.as_ref().map_or(0, |p| p.len()) + c.value.len())
            .sum::<usize>();
    }
    n
}

/// Reads one row of the base table.
async fn read_row(
    db: &Arc<Database>,
    t: &Arc<Table>,
    pk: &PartitionKey,
    ck: Option<&Bytes>,
) -> Result<Option<Row>> {
    let st = db.storage_table(t).map_err(engine_error)?;
    let mut r = PartitionRead::new(pk.clone(), now_secs());
    if let Some(ck) = ck {
        r.clustering = ClusteringFilter::Names(vec![Clustering::new([&ck[..]])]);
    }
    let parts = st.read(r).partitions().await.map_err(internal)?;
    Ok(parts
        .into_iter()
        .find(|p| p.key == *pk)
        .and_then(|p| p.rows.into_iter().next()))
}

fn key_of(key: &PrimaryKey) -> Result<(PartitionKey, Option<Bytes>)> {
    let pk = partition_key(&key.hash)?;
    let ck = key.range.as_ref().map(key_bytes).transpose()?;
    Ok((pk, ck))
}

pub async fn get(db: &Arc<Database>, t: &Arc<Table>, key: &PrimaryKey) -> Result<Option<Item>> {
    let (pk, ck) = key_of(key)?;
    Ok(read_row(db, t, &pk, ck.as_ref())
        .await?
        .map(|r| row_item(t, &pk.key, &r)))
}

/// The non-key attributes of an item as stored (top-level numbers normalised).
fn stored_attrs(schema: &TableSchema, item: &Item) -> Vec<(String, Vec<u8>)> {
    item.iter()
        .filter(|(k, _)| !schema.key.contains(k))
        .map(|(k, v)| (k.clone(), encode_value(v)))
        .collect()
}

/// Whether the table's stream post-images writes (its CDC reads every written row).
fn post_images(schema: &TableSchema) -> bool {
    schema.stream.as_ref().is_some_and(|s| {
        s.enabled && matches!(s.view_type.as_str(), "NEW_IMAGE" | "NEW_AND_OLD_IMAGES")
    })
}

/// A write; with `exact` (`alternator_streams_increased_compatibility`) it is made as with
/// post-images whatever the stream's view type, so the stream names events by the item's
/// existence and records no write that changes nothing.
pub async fn write(
    db: &Arc<Database>,
    schema: &TableSchema,
    t: &Arc<Table>,
    key: &PrimaryKey,
    f: WriteFn,
    exact: bool,
) -> Result<WriteOutcome> {
    write_marked(db, schema, t, key, f, exact, false).await
}

/// [`write`]; with `expiration`, a deletion is the TTL service's (its stream record says so).
pub async fn write_marked(
    db: &Arc<Database>,
    schema: &TableSchema,
    t: &Arc<Table>,
    key: &PrimaryKey,
    f: WriteFn,
    exact: bool,
    expiration: bool,
) -> Result<WriteOutcome> {
    let (pk, ck) = key_of(key)?;
    let _lock = db.lock_partition(&t.id, pk.token).await;
    let existing = read_row(db, t, &pk, ck.as_ref()).await?;
    let old = existing.as_ref().map(|r| row_item(t, &pk.key, r));
    let Some(m) = f(old.as_ref())? else {
        return Ok(WriteOutcome {
            new: old.clone(),
            old,
        });
    };
    let new = match &m {
        Mutation::Put { item, .. } => Some(item.clone()),
        Mutation::Update { item, sets, .. } => (old.is_some() || *sets).then(|| item.clone()),
        Mutation::Delete => None,
    };
    let images =
        post_images(schema) || (exact && schema.stream.as_ref().is_some_and(|s| s.enabled));
    let unchanged = match (&old, &new) {
        (None, None) => true,
        (Some(o), Some(n)) => stored_attrs(schema, o) == stored_attrs(schema, n),
        _ => false,
    };
    if images && unchanged {
        return Ok(WriteOutcome { old, new });
    }
    let ts = db.timestamp();
    let now = now_secs();
    let attrs_id = t
        .column(ATTRS)
        .map(|c| c.id)
        .ok_or_else(|| Error::internal("table without :attrs"))?;
    // An existing row keeps the clustering it was created with (`1.5` and `1.50` are one row).
    let clustering = match (&existing, &ck) {
        (Some(r), _) => r.clustering.clone(),
        (None, Some(c)) => Clustering::new([&c[..]]),
        (None, None) => Clustering::EMPTY,
    };
    let path = |name: &str| Bytes::copy_from_slice(name.as_bytes());
    let mut update = PartitionUpdate::new(pk.clone());
    match &m {
        Mutation::Put { item, rmw } => {
            let mut row = Row::new(clustering);
            if !(old.is_some() && (*rmw || images)) {
                row.liveness = LivenessInfo::live(ts);
            }
            let cells: Vec<Cell> = stored_attrs(schema, item)
                .into_iter()
                .map(|(k, v)| Cell::live(ts, Bytes::from(v)).with_path(path(&k)))
                .collect();
            row.columns.push(ColumnData::complex(
                attrs_id,
                DeletionTime::new(ts - 1, now),
                cells,
            ));
            update.rows.push(row);
        }
        Mutation::Update {
            item,
            sets,
            rmw,
            reads,
            touched,
        } => {
            let mut row = Row::new(clustering);
            if *sets && !(old.is_some() && (*rmw || *reads || images)) {
                row.liveness = LivenessInfo::live(ts);
            }
            let mut names: Vec<&str> = touched
                .iter()
                .map(String::as_str)
                .filter(|n| !schema.key.contains(n))
                .collect();
            let changed = |n: &str| old.as_ref().and_then(|o| o.get(n)) != item.get(n);
            for n in item.keys().chain(old.iter().flat_map(|o| o.keys())) {
                if !schema.key.contains(n) && !names.contains(&n.as_str()) && changed(n) {
                    names.push(n);
                }
            }
            names.sort_unstable();
            names.dedup();
            let cells: Vec<Cell> = names
                .into_iter()
                .map(|n| match item.get(n) {
                    Some(v) => Cell::live(ts, Bytes::from(encode_value(v))).with_path(path(n)),
                    None => Cell::tombstone(ts, now).with_path(path(n)),
                })
                .collect();
            if !cells.is_empty() {
                row.columns
                    .push(ColumnData::complex(attrs_id, DeletionTime::LIVE, cells));
            }
            update.rows.push(row);
        }
        Mutation::Delete => {
            if t.clustering.is_empty() {
                update.deletion = DeletionTime::new(ts, now);
            } else {
                // A deletion is keyed as requested (`1.5` deletes the row written as `1.50`, and
                // the CDC log shows `1.5`, as on ScyllaDB).
                let requested = ck
                    .as_ref()
                    .map_or(clustering, |c| Clustering::new([&c[..]]));
                update.rows.push(
                    Row::new(requested)
                        .with_deletion(RowDeletion::regular(DeletionTime::new(ts, now))),
                );
            }
        }
    }
    nosql_engine::statements::modify::apply_with_observers(db, t, vec![update])
        .await
        .map_err(engine_error)?;
    if expiration && matches!(m, Mutation::Delete) {
        super::streams::mark_expired(db, &schema.name, pk.token, ts).await?;
    }
    Ok(WriteOutcome { old, new })
}

/// The end tokens of a keyspace's token ranges, ascending: its tablets, or the node's vnodes.
fn token_ranges(db: &Database, keyspace: &str) -> Vec<i64> {
    let tablets = db.schema().keyspace(keyspace).and_then(|k| k.tablets);
    match tablets {
        Some(initial) => {
            let count = nosql_engine::cdc::tablet_count(initial) as i128;
            let width = (1i128 << 64) / count;
            (0..count)
                .map(|i| (i128::from(i64::MIN) + (i + 1) * width - 1) as i64)
                .collect()
        }
        None => {
            let mut t = nosql_engine::cdc::generation(db.config());
            t.sort_unstable();
            t
        }
    }
}

/// The clustering of `item` in `source` (its clustering key columns, all present).
fn clustering_of(source: &Table, item: &Item) -> Option<Clustering> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for c in source.clustering_columns() {
        parts.push(encode_key(item.get(&c.name)?)?);
    }
    Some(Clustering::new(parts.iter().map(|p| &p[..])))
}

fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut v = prefix.to_vec();
    while let Some(last) = v.pop() {
        if last < 0xFF {
            v.push(last + 1);
            return Some(v);
        }
    }
    None
}

/// The slice of a sort-key condition (on the first clustering column).
fn sort_slice(cond: &SortCondition) -> Result<Slice> {
    let c = |v: &Json| -> Result<Clustering> { Ok(Clustering::new([&key_bytes(v)?[..]])) };
    Ok(match cond {
        SortCondition::Eq(v) => Slice::prefix(c(v)?),
        SortCondition::Lt(v) => Slice::new(Bound::BOTTOM, Bound::exclusive_end(c(v)?)),
        SortCondition::Le(v) => Slice::new(Bound::BOTTOM, Bound::inclusive_end(c(v)?)),
        SortCondition::Gt(v) => Slice::new(Bound::exclusive_start(c(v)?), Bound::TOP),
        SortCondition::Ge(v) => Slice::new(Bound::inclusive_start(c(v)?), Bound::TOP),
        SortCondition::Between(a, b) => {
            Slice::new(Bound::inclusive_start(c(a)?), Bound::inclusive_end(c(b)?))
        }
        SortCondition::BeginsWith(v) => {
            let p = key_bytes(v)?;
            let end = match successor(&p) {
                Some(s) => Bound::exclusive_end(Clustering::new([&s[..]])),
                None => Bound::TOP,
            };
            Slice::new(Bound::inclusive_start(Clustering::new([&p[..]])), end)
        }
    })
}

pub async fn read_range(
    db: &Arc<Database>,
    source: &Arc<Table>,
    req: &RangeRequest,
) -> Result<RangePage> {
    // ScyllaDB ends a page after `query_tombstone_page_limit` tombstones, live rows or not.
    let mut limit = db
        .dialect()
        .is_scylla()
        .then(|| db.settings().scylla.query_tombstone_page_limit)
        .filter(|l| *l > 0);
    loop {
        let (mut page, cut) = read_range_once(db, source, req, limit).await?;
        let Some(cut) = cut else {
            return Ok(page);
        };
        // Resume after the cut: a clustering position, or a whole partition when the table
        // has no clustering key. A Query ends with its partition.
        let position = match (&req.scope, &cut.after) {
            (_, Some(c)) => Some(Row::new(c.clone())),
            (Scope::Tokens { .. }, None) if source.clustering.is_empty() => {
                Some(Row::new(Clustering::EMPTY))
            }
            (Scope::Partition { .. }, None) => return Ok(page),
            _ => None,
        };
        match position {
            Some(row) => {
                page.more = true;
                page.resume = Some(row_item(source, &cut.key.key, &row));
                return Ok(page);
            }
            // A position a LastEvaluatedKey can't name: end at the last item, or (nothing read
            // yet) read on without the limit.
            None if !page.items.is_empty() => {
                page.more = true;
                return Ok(page);
            }
            None => limit = None,
        }
    }
}

/// One read of [`read_range`]: the page, and where the read stopped at the tombstone limit.
async fn read_range_once(
    db: &Arc<Database>,
    source: &Arc<Table>,
    req: &RangeRequest,
    tombstone_limit: Option<u64>,
) -> Result<(RangePage, Option<nosql_storage::read::PageCut>)> {
    let st = db.storage_table(source).map_err(engine_error)?;
    let now = now_secs();
    let is_view = source.is_view();
    let after = req
        .start_after
        .as_ref()
        .map(|s| -> Result<(Option<PartitionKey>, Option<Clustering>)> {
            let pkc = source.partition_key_columns().next();
            let pk = match pkc.and_then(|c| s.get(&c.name)) {
                Some(v) => Some(partition_key(v)?),
                None => None,
            };
            Ok((pk, clustering_of(source, s)))
        })
        .transpose()?;
    let mut stream = match &req.scope {
        Scope::Partition { hash, sort } => {
            let pk = partition_key(hash)?;
            let mut slice = match sort {
                Some(c) => sort_slice(c)?,
                None => Slice::ALL,
            };
            // Resume after the start key, unless the slice already starts (ends) later.
            if let Some((_, Some(c))) = &after {
                let cmp = &*source.comparator;
                if req.forward {
                    let b = Bound::exclusive_start(c.clone());
                    if compare_bounds(cmp, &b, &slice.start).is_gt() {
                        slice.start = b;
                    }
                } else {
                    let b = Bound::exclusive_end(c.clone());
                    if compare_bounds(cmp, &b, &slice.end).is_lt() {
                        slice.end = b;
                    }
                }
            }
            let mut r = PartitionRead::new(pk, now);
            r.clustering = ClusteringFilter::Slices(vec![slice]);
            r.reversed = !req.forward;
            r.options.tombstone_page_limit = tombstone_limit;
            st.read(r)
        }
        Scope::Tokens { start, end } => {
            let mut r = RangeRead::all(now);
            r.start = KeyBound::Token {
                token: *start,
                after: false,
            };
            r.end = KeyBound::Token {
                token: *end,
                after: true,
            };
            if let Some((Some(pk), c)) = &after {
                r.resume = Some(Resume {
                    key: pk.clone(),
                    after: c.clone().filter(|_| !source.clustering.is_empty()),
                    returned_in_partition: 0,
                });
            }
            r.limits = Limits::default();
            r.options.tombstone_page_limit = tombstone_limit;
            st.scan(r)
        }
    };
    // ScyllaDB reads a scan's token ranges (tablets or vnodes) one after another, each with the
    // page's byte budget, until the page is full: a page stops inside a range that alone
    // exceeds the budget, or at the end of the range that fills it.
    let ranges = token_ranges(db, &source.keyspace);
    let range_of = |token: i64| ranges.partition_point(|end| *end < token);
    let mut page = RangePage::default();
    let (mut bytes, mut total) = (0usize, 0usize);
    let mut current: Option<usize> = None;
    'read: while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(internal)?;
        for row in &chunk.rows {
            let range = match req.scope {
                Scope::Tokens { .. } => range_of(chunk.key.token),
                Scope::Partition { .. } => 0,
            };
            if current != Some(range) {
                if current.is_some() && total >= req.max_bytes {
                    page.more = true;
                    break 'read;
                }
                bytes = 0;
                current = Some(range);
            }
            if page.items.len() >= req.limit || (bytes >= req.max_bytes && !page.items.is_empty()) {
                page.more = true;
                break 'read;
            }
            let item = row_item(source, &chunk.key.key, row);
            let size = if is_view {
                crate::size::item_size(&item)
            } else {
                row_size(source, &chunk.key.key, row)
            };
            bytes += size;
            total += size;
            page.items.push(item);
        }
    }
    if page.items.len() >= req.limit && !page.more {
        page.more = true;
    }
    let cut = if page.more {
        None
    } else {
        stream.metrics.page_cut()
    };
    Ok((page, cut))
}
