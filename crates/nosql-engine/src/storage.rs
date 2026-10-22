//! The bridge to `nosql-storage`: partition keys and tokens, per-table storage options, cell
//! paths of non-frozen collections and UDTs, and assembling stored rows back into column values.
//!
//! Cell layout (Cassandra's): a list element's path is a timeuuid, a set element's path is the
//! element (empty value), a map entry's path is the key, a non-frozen UDT field's path is its
//! index as a 2-byte big-endian short. Counter cells carry `[clock][total]`.

use std::sync::Arc;

use bytes::Bytes;
use nosql_storage::options::TableOptions;
use nosql_storage::{Cell, ColumnData, PartitionKey, Row};
use nosql_types::{CqlType, Dialect};

use crate::error::{Error, Result};
use crate::limits;
use crate::prepare::selection::CellMeta;
use crate::schema::{Column, Table};

/// The storage options of a table.
pub fn table_options(t: &Table, dialect: Dialect, memtable_flush_bytes: u64) -> TableOptions {
    TableOptions {
        comparator: t.comparator.clone(),
        reconcile: match dialect {
            Dialect::Scylla => nosql_storage::Reconcile::Scylla,
            Dialect::Cassandra => nosql_storage::Reconcile::Cassandra,
        },
        gc: t.params.gc_policy(),
        compaction: t.params.compaction_params(),
        bloom_filter_fp_chance: t.params.bloom_filter_fp_chance,
        strict_liveness: t.is_view(),
        memtable_flush_bytes,
        memtable_flush_period_ms: t.params.memtable_flush_period_in_ms.max(0) as u64,
        ..Default::default()
    }
}

/// Builds a partition key from its component values, checking emptiness and length.
pub fn partition_key(parts: &[Bytes], d: Dialect, for_write: bool) -> Result<PartitionKey> {
    if parts.len() == 1 && parts[0].is_empty() && (for_write || !d.is_scylla()) {
        return Err(Error::invalid("Key may not be empty"));
    }
    key_of_parts(parts, d)
}

/// The partition key of a view row: an empty value is a key there (a base row whose view key
/// column is `''` has an entry in both products).
pub fn view_partition_key(parts: &[Bytes], d: Dialect) -> Result<PartitionKey> {
    key_of_parts(parts, d)
}

/// A partition key's token. ScyllaDB gives the empty key (possible in views only) token 0, which
/// `token()` shows and its order follows; Cassandra the minimum token.
pub fn token_of(key: &[u8], d: Dialect) -> i64 {
    if key.is_empty() && d.is_scylla() {
        0
    } else {
        nosql_types::token(key)
    }
}

fn key_of_parts(parts: &[Bytes], d: Dialect) -> Result<PartitionKey> {
    let refs: Vec<&[u8]> = parts.iter().map(|p| &p[..]).collect();
    let key = nosql_types::partition_key(&refs).map_err(|e| Error::invalid(e.message))?;
    if key.len() > limits::MAX_KEY_LENGTH {
        let shown = if d.is_scylla() && parts.len() == 1 {
            key.len() + 2
        } else {
            key.len()
        };
        return Err(Error::invalid(format!(
            "Key length of {shown} is longer than maximum of {}",
            limits::MAX_KEY_LENGTH
        )));
    }
    let token = token_of(&key, d);
    Ok(PartitionKey::new(token, key))
}

/// A clustering key's length limit (64 KiB). ScyllaDB counts a composite key's 2-byte component
/// lengths too, shows them for a single component as well (as for partition keys), and checks
/// reads too; Cassandra checks writes only. A single 65535-byte component is accepted: ScyllaDB
/// refusing it is its issue #16772, which its suite marks as a bug.
pub fn check_clustering_length(c: &nosql_storage::Clustering, d: Dialect) -> Result<()> {
    let parts: Vec<usize> = c.components().map(<[u8]>::len).collect();
    let (n, shown) = match (d, parts.len()) {
        (Dialect::Scylla, 1) => (parts[0], parts[0] + 2),
        (Dialect::Scylla, _) => {
            let n = parts.iter().map(|p| p + 2).sum();
            (n, n)
        }
        (Dialect::Cassandra, _) => {
            let n = parts.iter().sum();
            (n, n)
        }
    };
    if n > limits::MAX_KEY_LENGTH {
        return Err(Error::invalid(format!(
            "Key length of {shown} is longer than maximum of {}",
            limits::MAX_KEY_LENGTH
        )));
    }
    Ok(())
}

/// The component values of a stored partition key.
pub fn key_components(t: &Table, key: &[u8]) -> Vec<Option<Bytes>> {
    if t.partition_key.len() == 1 {
        return vec![Some(Bytes::copy_from_slice(key))];
    }
    match nosql_types::split_composite(key) {
        Some(parts) => parts
            .into_iter()
            .map(|p| Some(Bytes::copy_from_slice(p)))
            .collect(),
        None => vec![None; t.partition_key.len()],
    }
}

/// The value of a complex column's cells, as a serialised whole value (`None` if no live cell).
pub fn assemble(ty: &CqlType, cells: &[&Cell]) -> Option<Bytes> {
    if cells.is_empty() {
        return None;
    }
    match ty {
        CqlType::List { .. } => Some(nosql_types::pack_collection(
            cells.iter().map(|c| &c.value[..]),
        )),
        CqlType::Set { .. } => Some(nosql_types::pack_collection(
            cells.iter().filter_map(|c| c.path.as_deref()),
        )),
        CqlType::Map { .. } => {
            Some(nosql_types::pack_map(cells.iter().filter_map(|c| {
                c.path.as_deref().map(|p| (p, &c.value[..]))
            })))
        }
        CqlType::Udt(def, _) => {
            let mut fields: Vec<Option<&[u8]>> = vec![None; def.fields.len()];
            for c in cells {
                if let Some(p) = &c.path
                    && p.len() == 2
                {
                    let i = u16::from_be_bytes([p[0], p[1]]) as usize;
                    if i < fields.len() {
                        fields[i] = Some(&c.value);
                    }
                }
            }
            // Every field is serialised, missing ones as null (`UserType.serializeForNativeProtocol`).
            Some(nosql_types::pack_fields(fields))
        }
        _ => cells.last().map(|c| c.value.clone()),
    }
}

/// A missing UDT field in per-cell write times (`ColumnTimestamps.addNoTimestamp`).
pub const NO_TIMESTAMP: i64 = i64::MIN;

/// The path of a UDT field cell.
pub fn field_path(index: usize) -> Bytes {
    Bytes::copy_from_slice(&(index as u16).to_be_bytes())
}

fn remaining_ttl(c: &Cell, now_s: i64) -> Option<i32> {
    if c.is_expiring() {
        Some((c.local_deletion_time - now_s).clamp(0, i32::MAX as i64) as i32)
    } else {
        None
    }
}

/// Reads column `col` from a stored row (live cells only, dropped incarnations skipped).
pub fn column_value(
    t: &Table,
    col: &Column,
    row: &Row,
    now_s: i64,
    meta: Option<&mut CellMeta>,
) -> Option<Bytes> {
    let dropped = t.dropped_before(&col.name);
    let visible = |c: &Cell| c.is_live(now_s) && dropped.is_none_or(|d| c.timestamp > d);
    match row.column(col.id)? {
        ColumnData::Simple { cell, .. } => {
            if !visible(cell) {
                return None;
            }
            if let Some(m) = meta {
                m.timestamp = Some(cell.timestamp);
                m.ttl = remaining_ttl(cell, now_s);
            }
            if cell.is_counter() {
                return cell.counter_value();
            }
            Some(cell.value.clone())
        }
        ColumnData::Complex {
            deletion, cells, ..
        } => {
            let live: Vec<&Cell> = cells
                .iter()
                .filter(|c| visible(c) && !deletion.deletes(c.timestamp))
                .collect();
            if let Some(m) = meta {
                m.cells = match &col.ty {
                    // One entry per field, `NO_TIMESTAMP` for missing ones (`Selector.add`).
                    CqlType::Udt(def, _) if !live.is_empty() => {
                        let mut per_field = vec![(NO_TIMESTAMP, None); def.fields.len()];
                        for c in &live {
                            if let Some(p) = c.path.as_deref().filter(|p| p.len() == 2) {
                                let i = u16::from_be_bytes([p[0], p[1]]) as usize;
                                if i < per_field.len() {
                                    per_field[i] = (c.timestamp, remaining_ttl(c, now_s));
                                }
                            }
                        }
                        per_field
                    }
                    _ => live
                        .iter()
                        .map(|c| (c.timestamp, remaining_ttl(c, now_s)))
                        .collect(),
                };
                m.timestamp = live.iter().map(|c| c.timestamp).max();
            }
            assemble(&col.ty, &live)
        }
    }
}

/// Whether a stored row has live data for the read (row marker or any live visible cell).
pub fn row_is_live(t: &Table, row: &Row, now_s: i64) -> bool {
    if row.liveness.is_live(now_s) {
        return true;
    }
    row.columns.iter().any(|cd| {
        let Some(col) = t.columns.iter().find(|c| c.id == cd.column()) else {
            return false;
        };
        let dropped = t.dropped_before(&col.name);
        match cd {
            ColumnData::Simple { cell, .. } => {
                cell.is_live(now_s) && dropped.is_none_or(|d| cell.timestamp > d)
            }
            ColumnData::Complex {
                cells, deletion, ..
            } => cells.iter().any(|c| {
                c.is_live(now_s)
                    && !deletion.deletes(c.timestamp)
                    && dropped.is_none_or(|d| c.timestamp > d)
            }),
        }
    })
}

/// Attaches (or refreshes the options of) a table in storage.
pub async fn attach(
    store: &nosql_storage::db::Database,
    t: &Table,
    d: Dialect,
    flush_bytes: u64,
) -> Result<Arc<nosql_storage::table::Table>> {
    storage_table(store, t.table_id(), table_options(t, d, flush_bytes))
        .await
        .map_err(Error::internal)
}

/// `nosql_storage::db::Database::table`, boxed: the storage future is only `Send` for concrete
/// lifetimes (rust-lang/rust#110338), so it is checked here rather than in callers' futures.
pub fn storage_table(
    store: &nosql_storage::db::Database,
    id: nosql_storage::manifest::TableId,
    opts: TableOptions,
) -> futures::future::BoxFuture<'_, anyhow::Result<Arc<nosql_storage::table::Table>>> {
    Box::pin(store.table(id, opts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let k = partition_key(&[Bytes::from_static(&[0, 0, 0, 1])], Dialect::Scylla, true).unwrap();
        assert_eq!(k.token, -4069959284402364209);
        assert_eq!(
            partition_key(&[Bytes::new()], Dialect::Cassandra, false)
                .unwrap_err()
                .message,
            "Key may not be empty"
        );
        assert!(partition_key(&[Bytes::new()], Dialect::Scylla, false).is_ok());
        let big = Bytes::from(vec![b'x'; 70_000]);
        assert_eq!(
            partition_key(std::slice::from_ref(&big), Dialect::Cassandra, true)
                .unwrap_err()
                .message,
            "Key length of 70000 is longer than maximum of 65535"
        );
        assert_eq!(
            partition_key(&[big], Dialect::Scylla, true)
                .unwrap_err()
                .message,
            "Key length of 70002 is longer than maximum of 65535"
        );
    }

    #[test]
    fn assembles_complex_values() {
        let l = CqlType::list(CqlType::Int, false);
        let c1 = Cell::live(1, Bytes::copy_from_slice(&1i32.to_be_bytes()))
            .with_path(Bytes::from_static(&[1; 16]));
        let c2 = Cell::live(1, Bytes::copy_from_slice(&2i32.to_be_bytes()))
            .with_path(Bytes::from_static(&[2; 16]));
        let v = assemble(&l, &[&c1, &c2]).unwrap();
        assert_eq!(nosql_types::collection_elements(&v).unwrap().len(), 2);
        let s = CqlType::set(CqlType::Text, false);
        let e = Cell::live(1, Bytes::new()).with_path(Bytes::from_static(b"a"));
        assert_eq!(
            nosql_types::collection_elements(&assemble(&s, &[&e]).unwrap()).unwrap(),
            vec![Some(&b"a"[..])]
        );
        let udt = CqlType::Udt(
            Arc::new(nosql_types::UdtDef::new(
                "k",
                "u",
                vec![("a".into(), CqlType::Int), ("b".into(), CqlType::Int)],
            )),
            false,
        );
        let f = Cell::live(1, Bytes::copy_from_slice(&7i32.to_be_bytes())).with_path(field_path(0));
        assert_eq!(
            nosql_types::tuple_fields(&assemble(&udt, &[&f]).unwrap())
                .unwrap()
                .len(),
            2
        );
        assert!(assemble(&l, &[]).is_none());
    }
}
