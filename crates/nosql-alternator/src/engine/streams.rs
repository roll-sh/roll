//! DynamoDB Streams from the CDC log, as ScyllaDB derives them.
//!
//! - A shard is a CDC stream of the table: one per tablet (or vnode), `H <stream set
//!   timestamp, hex ms>:<stream id, hex>`, listed in stream id order.
//! - A record is one group of log rows (one `cdc$time`): the pre-image (operation 0) is the
//!   old item, the post-image (9) the new one, the delta's operation names the event: a row or
//!   partition deletion (3, 4) is REMOVE; otherwise an insert (2) is INSERT and an update (1)
//!   MODIFY, except that OLD_IMAGE streams call any write of an existing item (one with a
//!   pre-image) MODIFY. Its SequenceNumber is the `cdc$time` position
//!   ([`crate::backend::timeuuid_position`]).

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use nosql_engine::Database;
use nosql_engine::cdc::Streams;
use nosql_engine::schema::Table;
use nosql_storage::read::{ClusteringFilter, PartitionRead};
use nosql_storage::{Bound, Clustering, Row, Slice};
use nosql_types::Dialect;

use super::catalog::{ATTRS, keyspace_name};
use super::engine_error;
use super::items::now_secs;
use crate::backend::*;
use crate::error::{Error, Result};
use crate::schema::TableSchema;
use crate::storage::{decode_key, decode_value};
use crate::value::Item;

/// The stream sets of a table's log, oldest first (whether or not CDC is on right now): one
/// for a vnodes keyspace; in a tablets keyspace one per tablet count of the table
/// ([`nosql_engine::cdc::generations`]).
fn stream_sets(db: &Database, table: &str) -> Option<(Arc<Table>, Arc<Table>, Vec<Streams>)> {
    let schema = db.schema();
    let ks = schema.keyspace(&keyspace_name(table))?;
    let base = ks.tables.get(table)?.clone();
    let log = ks.tables.get(&nosql_engine::cdc::log_name(table))?.clone();
    let sets = match ks.tablets {
        Some(_) => nosql_engine::cdc::generations(ks, &base, &log),
        None => vec![Streams::new(db.config(), &base, &log, false)],
    };
    Some((base, log, sets))
}

/// The current stream set of a table's log.
fn stream_set(db: &Database, table: &str) -> Option<(Arc<Table>, Arc<Table>, Streams)> {
    let (base, log, mut sets) = stream_sets(db, table)?;
    Some((base, log, sets.pop()?))
}

fn shard_name(streams: &Streams, id: &[u8; 16]) -> String {
    format!("H {:x}:{}", streams.timestamp_ms, hex::encode(id))
}

/// The shards of every generation, oldest generation first and in stream id order within one.
/// A shard of a later generation names as parent the shard of the previous one whose token
/// range holds its range's end (when tablets split, both halves name the tablet they come
/// from; when they merge, the first); a shard's children are the next generation's shards
/// whose ranges overlap its own.
pub fn shards(db: &Database, table: &str) -> Vec<StreamShard> {
    let Some((_, _, sets)) = stream_sets(db, table) else {
        return Vec::new();
    };
    let overlaps = |a: (i64, i64), b: (i64, i64)| a.0 < b.1 && b.0 < a.1;
    let mut out = Vec::new();
    for (g, streams) in sets.iter().enumerate() {
        let start = stream_position(streams.timestamp_ms, MIN_CLOCK);
        let end = sets
            .get(g + 1)
            .map(|n| stream_position(n.timestamp_ms, 0).saturating_sub(1));
        let prev = g.checked_sub(1).map(|p| &sets[p]);
        let next = sets.get(g + 1);
        let mut gen_shards: Vec<StreamShard> = (0..streams.ranges.len())
            .map(|i| {
                let id = streams.ranges[i].1;
                let range = streams.range(i);
                let parent = prev.map(|p| shard_name(p, &p.of(range.1)));
                let children = next
                    .map(|n| {
                        (0..n.ranges.len())
                            .filter(|&j| overlaps(range, n.range(j)))
                            .map(|j| shard_name(n, &n.ranges[j].1))
                            .collect()
                    })
                    .unwrap_or_default();
                StreamShard {
                    id: shard_name(streams, &id),
                    start,
                    end,
                    parent,
                    children,
                }
            })
            .collect();
        gen_shards.sort_by(|a, b| a.id.cmp(&b.id));
        gen_shards.dedup_by(|a, b| a.id == b.id);
        out.extend(gen_shards);
    }
    out
}

/// The stream id of a shard name.
fn stream_of(shard: &str) -> Option<[u8; 16]> {
    let (_, hex) = shard.strip_prefix("H ")?.split_once(':')?;
    let b = hex::decode(hex).ok()?;
    b.try_into().ok()
}

fn cell<'a>(log: &Table, row: &'a Row, column: &str) -> Option<&'a Bytes> {
    let c = log.column(column)?;
    row.cell(c.id)
        .filter(|c| !c.is_tombstone())
        .map(|c| &c.value)
}

/// The keys of a log row (base key columns are regular columns of the log).
fn keys(schema: &TableSchema, log: &Table, row: &Row) -> Item {
    let mut item = Item::new();
    for k in std::iter::once(&schema.key.hash).chain(schema.key.range.as_ref()) {
        if let Some(v) = cell(log, row, &k.name).and_then(|b| decode_key(k.ty, b)) {
            item.insert(k.name.clone(), v);
        }
    }
    item
}

/// An image row as an item: its keys and `:attrs`.
fn image(schema: &TableSchema, log: &Table, row: &Row) -> Item {
    let mut item = keys(schema, log, row);
    if let Some(m) = cell(log, row, ATTRS)
        && let Ok(entries) = nosql_types::map_entries(m)
    {
        for (k, v) in entries {
            if let (Ok(name), Some(v)) = (std::str::from_utf8(k), decode_value(v)) {
                item.insert(name.to_string(), v);
            }
        }
    }
    item
}

/// One record from the log rows of one `cdc$time`.
fn record(
    schema: &TableSchema,
    log: &Table,
    time: &[u8; 16],
    rows: &[Row],
) -> Option<StreamRecord> {
    let mut old = None;
    let mut new = None;
    let mut ops = Vec::new();
    let mut keys_row = None;
    for r in rows {
        let op = cell(log, r, "cdc$operation")
            .and_then(|b| b.first().copied())
            .unwrap_or(0) as i8;
        match op {
            0 => old = Some(image(schema, log, r)),
            9 => new = Some(image(schema, log, r)),
            _ => {
                ops.push(op);
                keys_row.get_or_insert(r);
            }
        }
    }
    let keys_row = keys_row.or(rows.first())?;
    // The TTL service's deletions have `cdc$ttl` = 0 ([`mark_expired`]).
    let expired = rows.iter().any(|r| {
        let op = cell(log, r, "cdc$operation").and_then(|b| b.first().copied());
        matches!(op, Some(3 | 4)) && cell(log, r, "cdc$ttl").is_some_and(|v| v[..] == [0u8; 8])
    });
    let view = schema
        .stream
        .as_ref()
        .map_or("KEYS_ONLY", |s| s.view_type.as_str());
    let kind = if ops.iter().any(|o| matches!(o, 3 | 4)) && !ops.contains(&2) {
        ChangeKind::Remove
    } else if view == "OLD_IMAGE" && old.is_some() {
        ChangeKind::Modify
    } else if ops.contains(&2) {
        ChangeKind::Insert
    } else {
        ChangeKind::Modify
    };
    if kind == ChangeKind::Remove {
        new = None;
    }
    let position = timeuuid_position(time);
    Some(StreamRecord {
        position,
        time_ms: position_ms(position),
        kind,
        keys: keys(schema, log, keys_row),
        old,
        new,
        expired,
    })
}

/// Marks the log rows of the TTL service's deletion at `ts` of the partition with `token`.
pub async fn mark_expired(db: &Arc<Database>, table: &str, token: i64, ts: i64) -> Result<()> {
    let Some((_, log, streams)) = stream_set(db, table) else {
        return Ok(());
    };
    let Some(ttl_col) = log.column("cdc$ttl").map(|c| c.id) else {
        return Ok(());
    };
    let id = streams.of(token);
    let pk = nosql_engine::storage::partition_key(
        &[Bytes::copy_from_slice(&id)],
        Dialect::Scylla,
        false,
    )
    .map_err(engine_error)?;
    let tick = (ts.max(0) as u64)
        .wrapping_mul(10)
        .wrapping_add(0x01B2_1DD2_1381_4000);
    let now = now_secs();
    let mut read = PartitionRead::new(pk.clone(), now);
    read.clustering = ClusteringFilter::Slices(vec![Slice::new(
        Bound::inclusive_start(Clustering::new([&min_timeuuid(tick)[..]])),
        Bound::TOP,
    )]);
    let parts = db
        .storage_table(&log)
        .map_err(engine_error)?
        .read(read)
        .partitions()
        .await
        .map_err(|e| Error::internal(e.to_string()))?;
    let mut update = nosql_storage::PartitionUpdate::new(pk.clone());
    for p in parts.into_iter().filter(|p| p.key == pk) {
        for r in p.rows {
            let Some(t) = r
                .clustering
                .get(0)
                .and_then(|b| <[u8; 16]>::try_from(&b[..]).ok())
            else {
                continue;
            };
            if (timeuuid_position(&t) >> 64) as u64 != tick {
                continue;
            }
            let op = cell(&log, &r, "cdc$operation").and_then(|b| b.first().copied());
            if !matches!(op, Some(3 | 4)) {
                continue;
            }
            let ttl = 86_400;
            update.rows.push(Row::new(r.clustering.clone()).with_cell(
                ttl_col,
                nosql_storage::Cell::expiring(
                    ts,
                    ttl,
                    now + i64::from(ttl),
                    Bytes::from_static(&[0; 8]),
                ),
            ));
        }
    }
    if !update.rows.is_empty() {
        db.storage_table(&log)
            .map_err(engine_error)?
            .apply(update)
            .await
            .map_err(|e| Error::internal(e.to_string()))?;
    }
    Ok(())
}

pub async fn records(
    db: &Arc<Database>,
    schema: &TableSchema,
    shard: &str,
    from: u128,
    inclusive: bool,
    until_ms: i64,
    limit: usize,
) -> Result<Vec<StreamRecord>> {
    let Some((_, log, _)) = stream_set(db, &schema.name) else {
        return Ok(Vec::new());
    };
    let Some(id) = stream_of(shard) else {
        return Err(Error::not_found("Invalid ShardId"));
    };
    let st = db.storage_table(&log).map_err(engine_error)?;
    let pk = nosql_engine::storage::partition_key(
        &[Bytes::copy_from_slice(&id)],
        Dialect::Scylla,
        false,
    )
    .map_err(engine_error)?;
    let mut read = PartitionRead::new(pk.clone(), now_secs());
    // From the first timeuuid of `from`'s 100 ns tick (exact positions are compared below).
    let tick = (from >> 64) as u64;
    if tick > 0 {
        let start = min_timeuuid(tick.saturating_sub(1));
        read.clustering = ClusteringFilter::Slices(vec![Slice::new(
            Bound::inclusive_start(Clustering::new([&start[..]])),
            Bound::TOP,
        )]);
    }
    let mut stream = st.read(read);
    let mut out: Vec<StreamRecord> = Vec::new();
    let mut group: Vec<Row> = Vec::new();
    let mut group_time: Option<[u8; 16]> = None;
    let flush = |time: &[u8; 16], rows: &[Row], out: &mut Vec<StreamRecord>| -> bool {
        let p = timeuuid_position(time);
        if (inclusive && p < from) || (!inclusive && p <= from) {
            return true;
        }
        if position_ms(p) > until_ms {
            return false;
        }
        if let Some(r) = record(schema, &log, time, rows) {
            out.push(r);
        }
        out.len() < limit
    };
    'read: while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::internal(e.to_string()))?;
        if chunk.key != pk {
            continue;
        }
        for row in chunk.rows {
            let Some(t) = row
                .clustering
                .get(0)
                .and_then(|b| <[u8; 16]>::try_from(&b[..]).ok())
            else {
                continue;
            };
            if group_time.is_some_and(|g| g != t) {
                let g = group_time.take().unwrap_or_default();
                if !flush(&g, &group, &mut out) {
                    group.clear();
                    break 'read;
                }
                group.clear();
            }
            group_time = Some(t);
            group.push(row);
        }
    }
    if let Some(g) = group_time
        && !group.is_empty()
        && out.len() < limit
    {
        flush(&g, &group, &mut out);
    }
    Ok(out)
}

/// The smallest timeuuid of a 100 ns tick (`minTimeuuid`'s clock and node bits).
fn min_timeuuid(ticks: u64) -> [u8; 16] {
    let mut u = [0u8; 16];
    u[0..4].copy_from_slice(&((ticks & 0xFFFF_FFFF) as u32).to_be_bytes());
    u[4..6].copy_from_slice(&(((ticks >> 32) & 0xFFFF) as u16).to_be_bytes());
    u[6..8].copy_from_slice(&((((ticks >> 48) & 0x0FFF) as u16) | 0x1000).to_be_bytes());
    u[8..16].copy_from_slice(&MIN_CLOCK.to_be_bytes());
    u
}
