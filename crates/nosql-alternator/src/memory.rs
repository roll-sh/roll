//! An in-memory [`Backend`], for tests and local use.
//!
//! It stores rows exactly as ScyllaDB's CQL table would (key columns in their CQL
//! serialisation, every other attribute as its `:attrs` blob, see [`crate::storage`]), so reads
//! show ScyllaDB's normalisation, partition identity and order. One mutex guards everything,
//! which trivially gives per-item read-modify-write isolation.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use nosql_types::CqlType;
use parking_lot::Mutex;

use crate::backend::*;
use crate::error::{Error, Result};
use crate::json::Json;
use crate::schema::{IndexKind, KeySchema, ScalarType, TableSchema};
use crate::storage::{decode_key, decode_value, encode_key, encode_value};
use crate::value::Item;

/// One CQL row: the clustering key bytes (if the table has a range key) and `:attrs`.
#[derive(Clone, Debug)]
struct Row {
    ck: Option<Vec<u8>>,
    attrs: BTreeMap<String, Vec<u8>>,
}

#[derive(Default)]
struct Table {
    schema: Option<Arc<TableSchema>>,
    /// (token, partition key bytes) → rows in clustering order.
    partitions: BTreeMap<(i64, Vec<u8>), Vec<Row>>,
    stream: Vec<StreamRecord>,
    next_sequence: u64,
}

#[derive(Default)]
pub struct MemoryBackend {
    tables: Mutex<HashMap<String, Table>>,
    /// Creation order, for ListTables.
    order: Mutex<Vec<String>>,
    /// `alternator_streams_increased_compatibility`.
    exact_streams: std::sync::atomic::AtomicBool,
}

impl MemoryBackend {
    pub fn new() -> MemoryBackend {
        MemoryBackend::default()
    }
}

fn cql_type(ty: ScalarType) -> CqlType {
    match ty {
        ScalarType::S => CqlType::Text,
        ScalarType::N => CqlType::Decimal,
        ScalarType::B => CqlType::Blob,
    }
}

fn key_bytes(v: &Json) -> Result<Vec<u8>> {
    encode_key(v).ok_or_else(|| Error::internal("invalid key value"))
}

fn row_to_item(schema: &TableSchema, pk: &[u8], row: &Row) -> Item {
    let mut item = Item::new();
    for (name, blob) in &row.attrs {
        if let Some(v) = decode_value(blob) {
            item.insert(name.clone(), v);
        }
    }
    if let Some(v) = decode_key(schema.key.hash.ty, pk) {
        item.insert(schema.key.hash.name.clone(), v);
    }
    if let (Some(r), Some(ck)) = (&schema.key.range, &row.ck)
        && let Some(v) = decode_key(r.ty, ck)
    {
        item.insert(r.name.clone(), v);
    }
    item
}

fn item_to_row(schema: &TableSchema, item: &Item, ck: Option<Vec<u8>>) -> Row {
    let attrs = item
        .iter()
        .filter(|(k, _)| !schema.key.contains(k))
        .map(|(k, v)| (k.clone(), encode_value(v)))
        .collect();
    Row { ck, attrs }
}

/// Approximate stored size of a row, for page byte limits.
fn row_size(row: &Row, pk: &[u8]) -> usize {
    pk.len()
        + row.ck.as_ref().map_or(0, |c| c.len())
        + row
            .attrs
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum::<usize>()
}

/// The (token, bytes) of a key value.
fn partition_of(v: &Json) -> Result<(i64, Vec<u8>)> {
    let b = key_bytes(v)?;
    Ok((nosql_types::token(&b), b))
}

/// A view row: the index key bytes and the item it came from.
struct ViewRow {
    part: (i64, Vec<u8>),
    /// Clustering components: index range (if any) then base keys not already in the key.
    ck: Vec<(CqlType, Vec<u8>)>,
    item: Item,
}

fn compare_ck(a: &[(CqlType, Vec<u8>)], b: &[(CqlType, Vec<u8>)]) -> Ordering {
    for ((t, x), (_, y)) in a.iter().zip(b) {
        match nosql_types::compare(t, x, y) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    a.len().cmp(&b.len())
}

/// The value of attribute `name` in `item` if it has the key type `ty`.
fn typed_key(item: &Item, name: &str, ty: ScalarType) -> Option<Vec<u8>> {
    let v = item.get(name)?;
    let (tag, p) = crate::storage::single_member(v)?;
    if tag != ty.name() || p.as_str().is_none_or(str::is_empty) {
        return None;
    }
    encode_key(v)
}

/// The clustering components of a view row: the index range key, then the base keys that are
/// not part of the index key.
fn view_ck(schema: &TableSchema, ikey: &KeySchema, item: &Item) -> Option<Vec<(CqlType, Vec<u8>)>> {
    let mut ck = Vec::new();
    if let Some(r) = &ikey.range {
        ck.push((cql_type(r.ty), typed_key(item, &r.name, r.ty)?));
    }
    for k in std::iter::once(&schema.key.hash).chain(schema.key.range.as_ref()) {
        if !ikey.contains(&k.name) {
            ck.push((cql_type(k.ty), typed_key(item, &k.name, k.ty)?));
        }
    }
    Some(ck)
}

impl Table {
    fn schema(&self) -> &TableSchema {
        self.schema.as_deref().expect("table schema")
    }

    fn items(&self) -> impl Iterator<Item = (&(i64, Vec<u8>), &Row)> {
        self.partitions
            .iter()
            .flat_map(|(k, rows)| rows.iter().map(move |r| (k, r)))
    }

    /// All rows of an index view, in view order.
    fn view(&self, index: &str) -> Result<(Vec<ViewRow>, KeySchema)> {
        let schema = self.schema();
        let idx = schema
            .index(index)
            .ok_or_else(|| Error::internal(format!("no index {index}")))?;
        let mut rows = Vec::new();
        for (part, row) in self.items() {
            let item = row_to_item(schema, &part.1, row);
            let Some(hash) = typed_key(&item, &idx.key.hash.name, idx.key.hash.ty) else {
                continue;
            };
            let Some(ck) = view_ck(schema, &idx.key, &item) else {
                continue;
            };
            let part = match idx.kind {
                IndexKind::Local => part.clone(),
                IndexKind::Global => (nosql_types::token(&hash), hash),
            };
            rows.push(ViewRow { part, ck, item });
        }
        rows.sort_by(|a, b| a.part.cmp(&b.part).then_with(|| compare_ck(&a.ck, &b.ck)));
        Ok((rows, idx.key.clone()))
    }

    fn find_row(
        &self,
        pk: &(i64, Vec<u8>),
        ck: Option<&[u8]>,
        ty: Option<ScalarType>,
    ) -> Option<&Row> {
        let rows = self.partitions.get(pk)?;
        match (ck, ty) {
            (Some(ck), Some(ty)) => rows.iter().find(|r| {
                r.ck.as_deref()
                    .is_some_and(|c| nosql_types::compare(&cql_type(ty), c, ck) == Ordering::Equal)
            }),
            _ => rows.first(),
        }
    }
}

/// Whether a key value of type `ty` meets a sort-key condition.
pub(crate) fn sort_matches_value(ty: ScalarType, v: &Json, cond: &SortCondition) -> bool {
    encode_key(v).is_some_and(|b| sort_matches(&cql_type(ty), &b, cond))
}

fn sort_matches(t: &CqlType, ck: &[u8], cond: &SortCondition) -> bool {
    let cmp = |v: &Json| encode_key(v).map(|b| nosql_types::compare(t, ck, &b));
    match cond {
        SortCondition::Eq(v) => cmp(v) == Some(Ordering::Equal),
        SortCondition::Lt(v) => cmp(v) == Some(Ordering::Less),
        SortCondition::Le(v) => matches!(cmp(v), Some(Ordering::Less | Ordering::Equal)),
        SortCondition::Gt(v) => cmp(v) == Some(Ordering::Greater),
        SortCondition::Ge(v) => matches!(cmp(v), Some(Ordering::Greater | Ordering::Equal)),
        SortCondition::Between(a, b) => {
            matches!(cmp(a), Some(Ordering::Greater | Ordering::Equal))
                && matches!(cmp(b), Some(Ordering::Less | Ordering::Equal))
        }
        SortCondition::BeginsWith(v) => encode_key(v).is_some_and(|p| ck.starts_with(&p)),
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    async fn create_table(&self, schema: TableSchema) -> Result<bool> {
        let mut tables = self.tables.lock();
        if tables.contains_key(&schema.name) {
            return Ok(false);
        }
        let name = schema.name.clone();
        tables.insert(
            name.clone(),
            Table {
                schema: Some(Arc::new(schema)),
                ..Table::default()
            },
        );
        self.order.lock().push(name);
        Ok(true)
    }

    async fn delete_table(&self, name: &str) -> Result<Option<TableSchema>> {
        let t = self.tables.lock().remove(name);
        self.order.lock().retain(|n| n != name);
        Ok(t.and_then(|t| t.schema).map(|s| (*s).clone()))
    }

    async fn table(&self, name: &str) -> Result<Option<Arc<TableSchema>>> {
        Ok(self.tables.lock().get(name).and_then(|t| t.schema.clone()))
    }

    async fn list_tables(&self) -> Result<Vec<String>> {
        Ok(self.order.lock().clone())
    }

    async fn alter_table(&self, schema: TableSchema) -> Result<bool> {
        let mut tables = self.tables.lock();
        let Some(t) = tables.get_mut(&schema.name) else {
            return Ok(false);
        };
        t.schema = Some(Arc::new(schema));
        Ok(true)
    }

    async fn get_item(&self, table: &TableSchema, key: &PrimaryKey) -> Result<Option<Item>> {
        let tables = self.tables.lock();
        let Some(t) = tables.get(&table.name) else {
            return Ok(None);
        };
        let schema = t.schema();
        let pk = partition_of(&key.hash)?;
        let ck = key.range.as_ref().map(key_bytes).transpose()?;
        let ty = schema.key.range.as_ref().map(|r| r.ty);
        Ok(t.find_row(&pk, ck.as_deref(), ty)
            .map(|r| row_to_item(schema, &pk.1, r)))
    }

    async fn write_item(
        &self,
        table: &TableSchema,
        key: &PrimaryKey,
        f: WriteFn,
    ) -> Result<WriteOutcome> {
        let mut tables = self.tables.lock();
        let Some(t) = tables.get_mut(&table.name) else {
            return Err(Error::not_found(format!(
                "Requested resource not found: Table: {} not found",
                table.name
            )));
        };
        let Some(schema) = t.schema.clone() else {
            return Err(Error::internal("table without schema"));
        };
        let pk = partition_of(&key.hash)?;
        let ck = key.range.as_ref().map(key_bytes).transpose()?;
        let ty = schema.key.range.as_ref().map(|r| r.ty);
        let old = t
            .find_row(&pk, ck.as_deref(), ty)
            .map(|r| row_to_item(&schema, &pk.1, r));
        let Some(m) = f(old.as_ref())? else {
            return Ok(WriteOutcome {
                new: old.clone(),
                old,
            });
        };
        let rows = t.partitions.entry(pk.clone()).or_default();
        let pos = match (&ck, ty) {
            (Some(ck), Some(ty)) => rows.iter().position(|r| {
                r.ck.as_deref()
                    .is_some_and(|c| nosql_types::compare(&cql_type(ty), c, ck) == Ordering::Equal)
            }),
            _ => (!rows.is_empty()).then_some(0),
        };
        let stored = match &m {
            Mutation::Put { item, .. } => Some(item),
            Mutation::Update { item, sets, .. } => (old.is_some() || *sets).then_some(item),
            Mutation::Delete => None,
        };
        let new = match stored {
            Some(item) => {
                // An existing row keeps the clustering bytes it was created with.
                let row_ck = pos.and_then(|p| rows[p].ck.clone()).or(ck.clone());
                let row = item_to_row(&schema, item, row_ck);
                match pos {
                    Some(p) => rows[p] = row,
                    None => {
                        let at = match (&row.ck, ty) {
                            (Some(c), Some(ty)) => rows
                                .iter()
                                .position(|r| {
                                    r.ck.as_deref().is_some_and(|x| {
                                        nosql_types::compare(&cql_type(ty), x, c)
                                            == Ordering::Greater
                                    })
                                })
                                .unwrap_or(rows.len()),
                            _ => rows.len(),
                        };
                        rows.insert(at, row);
                    }
                }
                Some(item.clone())
            }
            None => {
                if let Some(p) = pos {
                    rows.remove(p);
                }
                None
            }
        };
        if rows.is_empty() {
            t.partitions.remove(&pk);
        }
        // The stream sees the item as stored (top-level numbers normalised).
        let stored_new = new.as_ref().and_then(|_| {
            let rows = t.partitions.get(&pk)?;
            let r = match (&ck, ty) {
                (Some(ck), Some(ty)) => rows.iter().find(|r| {
                    r.ck.as_deref().is_some_and(|c| {
                        nosql_types::compare(&cql_type(ty), c, ck) == Ordering::Equal
                    })
                }),
                _ => rows.first(),
            };
            r.map(|r| row_to_item(&schema, &pk.1, r))
        });
        let exact = self
            .exact_streams
            .load(std::sync::atomic::Ordering::Relaxed);
        if let Some(st) = schema.stream.as_ref().filter(|s| s.enabled)
            && let Some(kind) = if exact {
                stream_event("NEW_IMAGE", &m, old.as_ref(), stored_new.as_ref())
            } else {
                stream_event(&st.view_type, &m, old.as_ref(), stored_new.as_ref())
            }
        {
            let mut keys = Item::new();
            keys.insert(schema.key.hash.name.clone(), key.hash.clone());
            if let (Some(r), Some(v)) = (&schema.key.range, &key.range) {
                keys.insert(r.name.clone(), v.clone());
            }
            // Keys as stored.
            for k in schema.key.names() {
                if let Some(v) = stored_new.as_ref().or(old.as_ref()).and_then(|i| i.get(k)) {
                    keys.insert(k.to_string(), v.clone());
                }
            }
            t.next_sequence += 1;
            let time_ms = chrono::Utc::now().timestamp_millis();
            t.stream.push(StreamRecord {
                position: stream_position(time_ms, t.next_sequence),
                time_ms,
                kind,
                keys,
                old: old.clone(),
                new: stored_new,
                expired: false,
            });
        }
        Ok(WriteOutcome { old, new })
    }

    async fn read_range(&self, table: &TableSchema, req: &RangeRequest) -> Result<RangePage> {
        let tables = self.tables.lock();
        let Some(t) = tables.get(&table.name) else {
            return Ok(RangePage::default());
        };
        let schema = t.schema();
        // Materialise the candidate rows as (partition, clustering, item), in read order.
        let mut rows: Vec<((i64, Vec<u8>), Vec<(CqlType, Vec<u8>)>, Item, usize)> = Vec::new();
        let (key, view): (KeySchema, Option<Vec<ViewRow>>) = match &req.index {
            Some(name) => {
                let (v, k) = t.view(name)?;
                (k, Some(v))
            }
            None => (schema.key.clone(), None),
        };
        let in_scope = |part: &(i64, Vec<u8>)| match &req.scope {
            Scope::Partition { hash, .. } => key_bytes(hash).is_ok_and(|b| b == part.1),
            Scope::Tokens { start, end } => part.0 >= *start && part.0 <= *end,
        };
        match view {
            None => {
                for (part, prow) in &t.partitions {
                    if !in_scope(part) {
                        continue;
                    }
                    for r in prow {
                        let ck = match (&schema.key.range, &r.ck) {
                            (Some(k), Some(c)) => vec![(cql_type(k.ty), c.clone())],
                            _ => vec![],
                        };
                        let size = row_size(r, &part.1);
                        rows.push((part.clone(), ck, row_to_item(schema, &part.1, r), size));
                    }
                }
            }
            Some(v) => {
                for r in v {
                    if !in_scope(&r.part) {
                        continue;
                    }
                    let size = crate::size::item_size(&r.item);
                    rows.push((r.part, r.ck, r.item, size));
                }
            }
        }
        // Sort-key condition (on the first clustering component).
        if let Scope::Partition {
            sort: Some(cond), ..
        } = &req.scope
        {
            rows.retain(|(_, ck, _, _)| ck.first().is_some_and(|(t, c)| sort_matches(t, c, cond)));
        }
        if !req.forward {
            // Reverse clustering order within each partition (Query reads one partition).
            rows.reverse();
        }
        // Resume after the start position.
        if let Some(start) = &req.start_after {
            let pos = |item: &Item| -> Option<((i64, Vec<u8>), Vec<(CqlType, Vec<u8>)>)> {
                let hash = typed_key(item, &key.hash.name, key.hash.ty)?;
                let part = if req.index.is_some()
                    && schema.index(req.index.as_deref()?)?.kind == IndexKind::Global
                {
                    (nosql_types::token(&hash), hash)
                } else {
                    let b = typed_key(item, &schema.key.hash.name, schema.key.hash.ty)?;
                    (nosql_types::token(&b), b)
                };
                let ck = match &req.index {
                    Some(_) => view_ck(schema, &key, item)?,
                    None => match &schema.key.range {
                        Some(k) => vec![(cql_type(k.ty), typed_key(item, &k.name, k.ty)?)],
                        None => vec![],
                    },
                };
                Some((part, ck))
            };
            if let Some((sp, sck)) = pos(start) {
                let after = |p: &(i64, Vec<u8>), ck: &[(CqlType, Vec<u8>)]| {
                    let o = p.cmp(&sp).then_with(|| compare_ck(ck, &sck));
                    if req.forward {
                        o == Ordering::Greater
                    } else {
                        o == Ordering::Less
                    }
                };
                rows.retain(|(p, ck, _, _)| after(p, ck));
            }
        }
        // The byte budget applies per partition (ScyllaDB reads token ranges separately), so a
        // scan only stops early inside a partition that alone exceeds it.
        let mut page = RangePage::default();
        let mut bytes = 0usize;
        let mut current: Option<(i64, Vec<u8>)> = None;
        for (part, _, item, size) in rows {
            if current.as_ref() != Some(&part) {
                bytes = 0;
                current = Some(part);
            }
            if page.items.len() >= req.limit || (bytes >= req.max_bytes && !page.items.is_empty()) {
                page.more = true;
                break;
            }
            bytes += size;
            page.items.push(item);
        }
        if page.items.len() >= req.limit && !page.more {
            page.more = true;
        }
        Ok(page)
    }

    fn set_streams_increased_compatibility(&self, on: bool) {
        self.exact_streams
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// One shard per table, named after the table id.
    async fn stream_shards(&self, table: &TableSchema) -> Result<Vec<StreamShard>> {
        let created = table.stream.as_ref().map_or(0, |s| s.created_at_ms);
        Ok(vec![StreamShard {
            id: format!("H {:x}:{}", created, table.id.simple()),
            start: stream_position(created, MIN_CLOCK),
            end: None,
            parent: None,
            children: Vec::new(),
        }])
    }

    async fn stream_records(
        &self,
        table: &TableSchema,
        _shard: &str,
        from: u128,
        inclusive: bool,
        until_ms: i64,
        limit: usize,
    ) -> Result<Vec<StreamRecord>> {
        let tables = self.tables.lock();
        let Some(t) = tables.get(&table.name) else {
            return Ok(Vec::new());
        };
        Ok(t.stream
            .iter()
            .filter(|r| {
                if inclusive {
                    r.position >= from
                } else {
                    r.position > from
                }
            })
            .filter(|r| r.time_ms <= until_ms)
            .take(limit)
            .cloned()
            .collect())
    }
}
