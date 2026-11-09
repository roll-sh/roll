//! Alternator tables as CQL schema, the way ScyllaDB 2026.3 lays them out (verified with
//! `DESCRIBE KEYSPACE` and the `system_schema` rows of the reference):
//!
//! ```cql
//! CREATE KEYSPACE alternator_T WITH replication = {'class': '…NetworkTopologyStrategy',
//!     'datacenter1': '1'} AND durable_writes = true AND tablets = {'enabled': true};
//! CREATE TABLE alternator_T.T (p text, c decimal, ":attrs" map<text, blob>,
//!     PRIMARY KEY (p, c)) WITH CLUSTERING ORDER BY (c ASC)
//!     AND cdc = {'delta': 'keys', 'enabled': 'true', 'postimage': 'true', 'preimage': 'full',
//!                'tablet_merge_blocked': 'true', 'ttl': '86400'}
//!     AND scylla_tags = {'k1': 'v1', 'system:table_creation_time': '1792540790693'};
//! CREATE MATERIALIZED VIEW alternator_T."T:gsi" AS SELECT * FROM alternator_T.T WHERE
//!     PRIMARY KEY (g, p, c) …  -- g: computed {"type":"alternator_extract_from_attrs",…}
//!     AND scylla_tags = {'system:spurious_range_key_added_to_gsi_and_user_didnt_specify_range_key': 'true'};
//! CREATE MATERIALIZED VIEW alternator_T."T!:lsi" AS SELECT * … PRIMARY KEY (p, l, c)
//!     AND scylla_tags = {'system:synchronous_view_updates': 'true'};
//! ```
//!
//! - `scylla_tags` (a table extension) holds the user tags and Alternator's `system:` tags:
//!   `table_creation_time`, `provisioned_rcu` / `provisioned_wcu` (PROVISIONED billing),
//!   `ttl_attribute`, `write_isolation`, `initial_tablets`.
//! - `initial_tablets` decides the keyspace: a number is a tablets keyspace (`initial` when
//!   positive), anything else vnodes.
//! - A stream is the table's CDC log; disabling it keeps the log (`enabled: false`).
//! - A vector index is a `CUSTOM` index `USING 'vector_index'` on the attribute name (there is
//!   no such column); the table gets a CDC log for it.
//!
//! [`load`] reads a [`TableSchema`] back from that schema; what ScyllaDB doesn't store
//! (projections as requested, index throughput, backfill and stream visibility times) comes
//! from the last schema Alternator wrote, when it is still the same table.

use std::collections::BTreeMap;
use std::sync::Arc;

use nosql_engine::schema::params::{parse_scylla_map_blob, scylla_map_blob};
use nosql_engine::schema::{
    Column, ColumnKind, IndexMeta, Keyspace, KeyspaceKind, Replication, Table, TableFlags,
    TableKind, TableParams, ViewMeta,
};
use nosql_engine::views::spec::Extract;
use nosql_storage::manifest::TableId;
use nosql_types::{CqlType, Dialect};

use crate::json::Json;
use crate::schema::{
    IndexKind, IndexSchema, KeyAttr, KeySchema, ScalarType, StreamSpec, TableSchema, VectorIndex,
};

pub use crate::storage::ATTRS_COLUMN as ATTRS;

pub const TAGS: &str = "scylla_tags";
const CREATION_TIME: &str = "system:table_creation_time";
const RCU: &str = "system:provisioned_rcu";
const WCU: &str = "system:provisioned_wcu";
const TTL: &str = "system:ttl_attribute";
const SPURIOUS_RANGE: &str =
    "system:spurious_range_key_added_to_gsi_and_user_didnt_specify_range_key";
const SYNCHRONOUS: &str = "system:synchronous_view_updates";
/// CDC log rows expire after a day (`cdc = {'ttl': '86400'}`).
const LOG_TTL: &str = "86400";

pub fn cql_type(t: ScalarType) -> CqlType {
    match t {
        ScalarType::S => CqlType::Text,
        ScalarType::N => CqlType::Decimal,
        ScalarType::B => CqlType::Blob,
    }
}

pub fn scalar(t: &CqlType) -> Option<ScalarType> {
    match t {
        CqlType::Text | CqlType::Ascii => Some(ScalarType::S),
        CqlType::Decimal => Some(ScalarType::N),
        CqlType::Blob => Some(ScalarType::B),
        _ => None,
    }
}

fn attrs_type() -> CqlType {
    CqlType::map(CqlType::Text, CqlType::Blob, false)
}

fn column(name: &str, ty: CqlType, kind: ColumnKind, position: i32) -> Column {
    Column {
        name: name.to_string(),
        ty,
        kind,
        position,
        reversed: false,
        id: 0,
    }
}

/// Column ids in canonical order, as a CREATE assigns them.
fn with_ids(mut cols: Vec<Column>) -> Vec<Column> {
    cols.sort_by(nosql_engine::schema::canonical_order);
    for (i, c) in cols.iter_mut().enumerate() {
        c.id = i as u32;
    }
    cols
}

/// The keyspace of Alternator table `table`.
pub fn keyspace_name(table: &str) -> String {
    crate::storage::keyspace_name(table)
}

/// The CDC options of a stream: what each view type logs.
fn cdc_options(view_type: &str, enabled: bool) -> BTreeMap<String, String> {
    let (pre, post) = match view_type {
        "NEW_AND_OLD_IMAGES" => ("full", "true"),
        "NEW_IMAGE" => ("false", "true"),
        "OLD_IMAGE" => ("full", "false"),
        _ => ("false", "false"),
    };
    [
        ("delta", "keys"),
        ("enabled", if enabled { "true" } else { "false" }),
        ("postimage", post),
        ("preimage", pre),
        ("tablet_merge_blocked", "true"),
        ("ttl", LOG_TTL),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn view_type_of(cdc: &BTreeMap<String, String>) -> &'static str {
    let on = |k: &str, v: &str| cdc.get(k).is_some_and(|x| x.eq_ignore_ascii_case(v));
    match (
        on("preimage", "full") || on("preimage", "true"),
        on("postimage", "true"),
    ) {
        (true, true) => "NEW_AND_OLD_IMAGES",
        (false, true) => "NEW_IMAGE",
        (true, false) => "OLD_IMAGE",
        (false, false) => "KEYS_ONLY",
    }
}

/// The tags a table stores in `scylla_tags`.
fn stored_tags(s: &TableSchema) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = s.tags.iter().cloned().collect();
    m.insert(CREATION_TIME.into(), s.created_at_ms.to_string());
    if s.billing_mode == "PROVISIONED" {
        m.insert(RCU.into(), s.provisioned.0.to_string());
        m.insert(WCU.into(), s.provisioned.1.to_string());
    }
    if let Some(a) = &s.ttl_attribute {
        m.insert(TTL.into(), a.clone());
    }
    m
}

/// The tags of `t` (`scylla_tags`).
pub fn tags_of(t: &Table) -> BTreeMap<String, String> {
    t.params
        .extensions
        .get(TAGS)
        .and_then(|b| parse_scylla_map_blob(b))
        .unwrap_or_default()
}

fn set_tags(p: &mut TableParams, tags: &BTreeMap<String, String>) {
    if tags.is_empty() {
        p.extensions.remove(TAGS);
    } else {
        p.extensions.insert(TAGS.into(), scylla_map_blob(tags));
    }
}

/// A new table's options: CQL's, without the `tombstone_gc` extension CQL tables get.
fn alternator_params() -> TableParams {
    let mut p = TableParams::defaults(Dialect::Scylla, false);
    p.tombstone_gc.clear();
    p
}

fn base_params(s: &TableSchema, previous: Option<&TableParams>) -> TableParams {
    let mut p = previous.cloned().unwrap_or_else(alternator_params);
    set_tags(&mut p, &stored_tags(s));
    p.scylla_cdc = match &s.stream {
        Some(st) if st.enabled || previous.is_some_and(|p| !p.scylla_cdc.is_empty()) => {
            cdc_options(&st.view_type, st.enabled)
        }
        _ => BTreeMap::new(),
    };
    p
}

/// The base table of `s`.
fn base_table(s: &TableSchema, previous: Option<&Table>) -> Table {
    let mut cols = vec![column(
        &s.key.hash.name,
        cql_type(s.key.hash.ty),
        ColumnKind::PartitionKey,
        0,
    )];
    if let Some(r) = &s.key.range {
        cols.push(column(&r.name, cql_type(r.ty), ColumnKind::Clustering, 0));
    }
    cols.push(column(ATTRS, attrs_type(), ColumnKind::Regular, -1));
    let params = base_params(s, previous.map(|t| &t.params));
    let indexes: Vec<IndexMeta> = s
        .vector_indexes
        .iter()
        .map(|v| IndexMeta {
            name: v.name.clone(),
            kind: "CUSTOM".into(),
            options: [
                ("class_name", "vector_index".to_string()),
                ("dimensions", v.dimensions.to_string()),
                ("similarity_function", v.similarity.clone()),
                ("target", v.attribute.clone()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        })
        .collect();
    match previous {
        Some(t) => {
            let mut n = t.rebuild(t.columns.clone(), params);
            n.indexes = indexes;
            n
        }
        None => {
            let mut t = Table::new(
                keyspace_name(&s.name),
                &s.name,
                *s.id.as_bytes(),
                nosql_engine::uuid::timeuuid(),
                with_ids(cols),
                params,
                TableFlags {
                    compound: true,
                    ..Default::default()
                },
                TableKind::Regular,
            );
            t.indexes = indexes;
            t
        }
    }
}

/// The view implementing index `idx` of `s`.
fn index_view(s: &TableSchema, base: &Table, idx: &IndexSchema) -> Table {
    let ty = |name: &str| s.attr_type(name).map(cql_type).unwrap_or(CqlType::Text);
    let mut cols = Vec::new();
    let mut computed = Vec::new();
    let mut keys: Vec<&str> = idx.key.names().collect();
    for k in s.key.names() {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    for (i, name) in keys.iter().enumerate() {
        let (kind, pos) = if i == 0 {
            (ColumnKind::PartitionKey, 0)
        } else {
            (ColumnKind::Clustering, i as i32 - 1)
        };
        cols.push(column(name, ty(name), kind, pos));
        if !s.key.contains(name) {
            let t = s.attr_type(name).unwrap_or(ScalarType::S);
            computed.push((name.to_string(), Extract::json(name, t.name())));
        }
    }
    cols.push(column(ATTRS, attrs_type(), ColumnKind::Regular, -1));
    let mut params = alternator_params();
    let mut tags = BTreeMap::new();
    match idx.kind {
        IndexKind::Global if idx.key.range.is_none() => {
            tags.insert(SPURIOUS_RANGE.to_string(), "true".to_string());
        }
        IndexKind::Local => {
            tags.insert(SYNCHRONOUS.to_string(), "true".to_string());
        }
        _ => {}
    }
    set_tags(&mut params, &tags);
    let mut v = Table::new(
        keyspace_name(&s.name),
        idx.view_name(&s.name),
        nosql_engine::uuid::timeuuid(),
        nosql_engine::uuid::timeuuid(),
        with_ids(cols),
        params,
        TableFlags {
            compound: true,
            ..Default::default()
        },
        TableKind::Regular,
    );
    v.view = Some(ViewMeta {
        base_table_id: base.id,
        base_table_name: base.name.clone(),
        where_clause: String::new(),
        include_all_columns: true,
        computed,
    });
    v
}

/// A keyspace definition and what committing it attaches, drops and backfills.
pub struct Plan {
    pub keyspace: Keyspace,
    pub base: Arc<Table>,
    pub attach: Vec<Arc<Table>>,
    pub drop: Vec<TableId>,
    /// New views (backfilled from the base after the commit).
    pub build: Vec<Arc<Table>>,
}

/// The keyspace of `s`, created or changed from `existing`.
pub fn plan(s: &TableSchema, existing: Option<&Keyspace>, datacenter: &str) -> Plan {
    let mut ks = match existing {
        Some(k) => k.clone(),
        None => {
            let mut k = Keyspace::new(
                keyspace_name(&s.name),
                Replication::new("NetworkTopologyStrategy", &[(datacenter, "1")]),
                KeyspaceKind::User,
            );
            k.tablets = if s.tablets {
                Some(crate::ops::vector::initial_tablets(&s.tags).unwrap_or(0))
            } else {
                None
            };
            k
        }
    };
    let previous = existing.and_then(|k| k.tables.get(&s.name)).cloned();
    let base = Arc::new(base_table(s, previous.as_deref()));
    let mut attach = Vec::new();
    if previous
        .as_ref()
        .is_none_or(|p| p.params != base.params || p.indexes != base.indexes)
    {
        attach.push(base.clone());
    }
    ks.tables.insert(base.name.clone(), base.clone());
    // Views.
    let mut drop = Vec::new();
    let mut build = Vec::new();
    let wanted: Vec<(String, &IndexSchema)> = s
        .gsis
        .iter()
        .chain(&s.lsis)
        .map(|i| (i.view_name(&s.name), i))
        .collect();
    let old_views: Vec<Arc<Table>> = ks
        .tables
        .values()
        .filter(|t| t.view.as_ref().is_some_and(|v| v.base_table_id == base.id))
        .cloned()
        .collect();
    for v in &old_views {
        if !wanted.iter().any(|(n, _)| *n == v.name) {
            ks.tables.remove(&v.name);
            drop.push(v.table_id());
        }
    }
    for (name, idx) in &wanted {
        if old_views.iter().any(|v| v.name == *name) {
            continue;
        }
        let v = Arc::new(index_view(s, &base, idx));
        ks.tables.insert(name.clone(), v.clone());
        attach.push(v.clone());
        build.push(v);
    }
    // The CDC log: for a stream (kept once created) or for vector indexes.
    let log_name = nosql_engine::cdc::log_name(&s.name);
    let wants_log = s.stream.is_some() || !s.vector_indexes.is_empty();
    match ks.tables.get(&log_name) {
        Some(log) => {
            if let Ok(Some(l)) = nosql_engine::cdc::follow_base(log, &base, Dialect::Scylla, nosql_types::temporal::now_millis() * 1000) {
                let l = Arc::new(l);
                ks.tables.insert(log_name.clone(), l.clone());
                attach.push(l);
            }
        }
        None if wants_log => {
            if let Ok(mut l) = nosql_engine::cdc::log_table(&base, Dialect::Scylla) {
                // The log's id carries the stream's creation time (its ARN and label).
                if let Some(st) = &s.stream {
                    l.id = timeuuid_at(st.created_at_ms);
                }
                let l = Arc::new(l);
                ks.tables.insert(log_name.clone(), l.clone());
                attach.push(l);
            }
        }
        None => {}
    }
    Plan {
        keyspace: ks,
        base,
        attach,
        drop,
        build,
    }
}

/// A timeuuid at `ms` (random clock and node bits).
fn timeuuid_at(ms: i64) -> [u8; 16] {
    let t = (ms.max(0) as u64)
        .wrapping_mul(10_000)
        .wrapping_add(0x01B2_1DD2_1381_4000);
    let mut u = nosql_engine::uuid::timeuuid();
    u[0..4].copy_from_slice(&((t & 0xFFFF_FFFF) as u32).to_be_bytes());
    u[4..6].copy_from_slice(&(((t >> 32) & 0xFFFF) as u16).to_be_bytes());
    u[6..8].copy_from_slice(&((((t >> 48) & 0x0FFF) as u16) | 0x1000).to_be_bytes());
    u
}

/// The milliseconds since the epoch of a timeuuid.
fn timeuuid_ms(id: &[u8; 16]) -> i64 {
    nosql_engine::cdc::unix_millis(id)
}

/// Reads table `name` back from its keyspace.
pub fn load(ks: &Keyspace, name: &str) -> Option<TableSchema> {
    let t = ks.tables.get(name)?;
    if t.is_view() {
        return None;
    }
    let attr = |c: &Column| -> Option<KeyAttr> {
        Some(KeyAttr {
            name: c.name.clone(),
            ty: scalar(&c.ty)?,
        })
    };
    let key = KeySchema {
        hash: attr(t.partition_key_columns().next()?)?,
        range: match t.clustering_columns().next() {
            Some(c) => Some(attr(c)?),
            None => None,
        },
    };
    let tags = tags_of(t);
    let created_at_ms = tags
        .get(CREATION_TIME)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| timeuuid_ms(&t.id));
    let provisioned = match (tags.get(RCU), tags.get(WCU)) {
        (Some(r), Some(w)) => Some((r.parse().unwrap_or(0), w.parse().unwrap_or(0))),
        _ => None,
    };
    let user_tags: Vec<(String, String)> = tags
        .iter()
        .filter(|(k, _)| {
            !k.starts_with("system:")
                || *k == "system:write_isolation"
                || *k == "system:initial_tablets"
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut attribute_definitions: Vec<KeyAttr> = key
        .names()
        .filter_map(|n| t.column(n).and_then(attr))
        .collect();
    let mut gsis = Vec::new();
    let mut lsis = Vec::new();
    for v in ks.tables.values() {
        let Some(meta) = &v.view else { continue };
        if meta.base_table_id != t.id {
            continue;
        }
        let (kind, index) = if let Some(n) = v.name.strip_prefix(&format!("{name}!:")) {
            (IndexKind::Local, n)
        } else if let Some(n) = v.name.strip_prefix(&format!("{name}:")) {
            (IndexKind::Global, n)
        } else {
            continue;
        };
        let Some(hash) = v.partition_key_columns().next().and_then(attr) else {
            continue;
        };
        let spurious = tags_of(v).contains_key(SPURIOUS_RANGE);
        let range = match kind {
            IndexKind::Global if spurious => None,
            _ => v.clustering_columns().next().and_then(attr),
        };
        let ikey = KeySchema { hash, range };
        for a in std::iter::once(&ikey.hash).chain(ikey.range.as_ref()) {
            if !attribute_definitions.iter().any(|x| x.name == a.name) {
                attribute_definitions.push(a.clone());
            }
        }
        let idx = IndexSchema {
            name: index.to_string(),
            kind,
            key: ikey,
            projection: Json::object().with("ProjectionType", Json::str("ALL")),
            provisioned: None,
            warm: None,
            ready_at_ms: 0,
        };
        match kind {
            IndexKind::Global => gsis.push(idx),
            IndexKind::Local => lsis.push(idx),
        }
    }
    let stream = ks
        .tables
        .get(&nosql_engine::cdc::log_name(name))
        .map(|log| {
            let enabled = t
                .params
                .scylla_cdc
                .get("enabled")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"));
            StreamSpec {
                view_type: view_type_of(&t.params.scylla_cdc).to_string(),
                created_at_ms: if t.params.scylla_cdc.is_empty() {
                    created_at_ms
                } else {
                    timeuuid_ms(&log.id).max(created_at_ms)
                },
                enabled,
                visible_at_ms: 0,
                reported_disabled: false,
            }
        });
    let vector_indexes = t
        .indexes
        .iter()
        .filter(|i| i.options.get("class_name").map(String::as_str) == Some("vector_index"))
        .map(|i| VectorIndex {
            name: i.name.clone(),
            attribute: i.options.get("target").cloned().unwrap_or_default(),
            dimensions: i
                .options
                .get("dimensions")
                .and_then(|d| d.parse().ok())
                .unwrap_or(0),
            similarity: i
                .options
                .get("similarity_function")
                .cloned()
                .unwrap_or_else(|| "COSINE".into()),
        })
        .collect();
    Some(TableSchema {
        name: name.to_string(),
        id: uuid::Uuid::from_bytes(t.id),
        created_at_ms,
        key,
        attribute_definitions,
        gsis,
        lsis,
        billing_mode: if provisioned.is_some() {
            "PROVISIONED".into()
        } else {
            "PAY_PER_REQUEST".into()
        },
        billing_changed_ms: created_at_ms,
        provisioned: provisioned.unwrap_or((0, 0)),
        stream,
        tags: user_tags,
        ttl_attribute: tags.get(TTL).cloned(),
        options: Vec::new(),
        vector_indexes,
        tablets: ks.tablets.is_some(),
    })
}

/// `loaded` with what ScyllaDB doesn't store taken from `written`, the last schema Alternator
/// wrote for the same table.
pub fn merge(mut loaded: TableSchema, written: &TableSchema) -> TableSchema {
    if written.id != loaded.id {
        return loaded;
    }
    for (mine, theirs) in [
        (&mut loaded.gsis, &written.gsis),
        (&mut loaded.lsis, &written.lsis),
    ] {
        // Creation order, as written.
        mine.sort_by_key(|i| theirs.iter().position(|w| w.name == i.name));
        for i in mine.iter_mut() {
            if let Some(w) = theirs.iter().find(|w| w.name == i.name && w.key == i.key) {
                i.projection = w.projection.clone();
                i.provisioned = w.provisioned;
                i.warm = w.warm.clone();
                i.ready_at_ms = w.ready_at_ms;
            }
        }
    }
    if let (Some(l), Some(w)) = (&mut loaded.stream, &written.stream)
        && l.view_type == w.view_type
        && l.enabled == w.enabled
    {
        l.created_at_ms = w.created_at_ms;
        l.visible_at_ms = w.visible_at_ms;
        l.reported_disabled = w.reported_disabled;
    }
    if loaded.billing_mode == written.billing_mode {
        loaded.billing_changed_ms = written.billing_changed_ms;
    }
    let mut ordered: Vec<(String, String)> = Vec::with_capacity(loaded.tags.len());
    for (k, v) in &written.tags {
        if loaded.tags.iter().any(|(x, y)| x == k && y == v) {
            ordered.push((k.clone(), v.clone()));
        }
    }
    for t in &loaded.tags {
        if !ordered.contains(t) {
            ordered.push(t.clone());
        }
    }
    loaded.tags = ordered;
    if loaded.vector_indexes.len() == written.vector_indexes.len()
        && loaded
            .vector_indexes
            .iter()
            .all(|v| written.vector_indexes.contains(v))
    {
        loaded.vector_indexes = written.vector_indexes.clone();
    }
    loaded.options = written.options.clone();
    loaded.attribute_definitions = {
        let mut a = written.attribute_definitions.clone();
        a.retain(|x| loaded.attribute_definitions.contains(x));
        for x in &loaded.attribute_definitions {
            if !a.contains(x) {
                a.push(x.clone());
            }
        }
        a
    };
    loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(name: &str, ty: ScalarType) -> KeyAttr {
        KeyAttr {
            name: name.into(),
            ty,
        }
    }

    fn index(name: &str, kind: IndexKind, hash: KeyAttr, range: Option<KeyAttr>) -> IndexSchema {
        IndexSchema {
            name: name.into(),
            kind,
            key: KeySchema { hash, range },
            projection: Json::object().with("ProjectionType", Json::str("ALL")),
            provisioned: None,
            warm: None,
            ready_at_ms: 0,
        }
    }

    fn schema() -> TableSchema {
        TableSchema {
            name: "t-1.x".into(),
            id: uuid::Uuid::from_bytes(nosql_engine::uuid::timeuuid()),
            created_at_ms: 1_792_540_790_693,
            key: KeySchema {
                hash: attr("p", ScalarType::S),
                range: Some(attr("c", ScalarType::N)),
            },
            attribute_definitions: vec![
                attr("p", ScalarType::S),
                attr("c", ScalarType::N),
                attr("g", ScalarType::B),
                attr("l", ScalarType::S),
            ],
            gsis: vec![index(
                "gsi",
                IndexKind::Global,
                attr("g", ScalarType::B),
                None,
            )],
            lsis: vec![index(
                "lsi",
                IndexKind::Local,
                attr("p", ScalarType::S),
                Some(attr("l", ScalarType::S)),
            )],
            billing_mode: "PROVISIONED".into(),
            billing_changed_ms: 1_792_540_790_693,
            provisioned: (5, 7),
            stream: Some(StreamSpec {
                view_type: "OLD_IMAGE".into(),
                created_at_ms: 1_792_540_790_693,
                enabled: true,
                visible_at_ms: 0,
                reported_disabled: false,
            }),
            tags: vec![
                ("k".into(), "v".into()),
                ("system:write_isolation".into(), "forbid_rmw".into()),
            ],
            ttl_attribute: Some("exp".into()),
            options: Vec::new(),
            vector_indexes: Vec::new(),
            tablets: true,
        }
    }

    #[test]
    fn tables_round_trip_through_the_cql_schema() {
        let s = schema();
        let p = plan(&s, None, "datacenter1");
        assert_eq!(p.keyspace.name, "alternator_t-1.x");
        assert_eq!(p.keyspace.tablets, Some(0));
        let names: Vec<&str> = p.keyspace.tables.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["t-1.x", "t-1.x!:lsi", "t-1.x:gsi", "t-1.x_scylla_cdc_log"]
        );
        assert_eq!(p.build.len(), 2, "both views are backfilled");
        // The base: key columns and :attrs; the tags and CDC options as ScyllaDB stores them.
        let base = &p.keyspace.tables["t-1.x"];
        let cols: Vec<(&str, String)> = base
            .columns
            .iter()
            .map(|c| (c.name.as_str(), nosql_types::cql_name(&c.ty)))
            .collect();
        assert_eq!(
            cols,
            [
                ("p", "text".into()),
                ("c", "decimal".into()),
                (":attrs", "map<text, blob>".into())
            ]
        );
        let tags = tags_of(base);
        assert_eq!(tags["system:provisioned_rcu"], "5");
        assert_eq!(tags["system:ttl_attribute"], "exp");
        assert_eq!(tags["system:table_creation_time"], "1792540790693");
        assert_eq!(base.params.scylla_cdc["preimage"], "full");
        assert!(base.params.tombstone_gc.is_empty());
        // The GSI: computed key from :attrs, base keys as clustering, the spurious-range tag.
        let gsi = &p.keyspace.tables["t-1.x:gsi"];
        let key: Vec<&str> = gsi
            .partition_key_columns()
            .chain(gsi.clustering_columns())
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(key, ["g", "p", "c"]);
        let meta = gsi.view.as_ref().unwrap();
        assert_eq!(
            meta.computed,
            [(
                "g".to_string(),
                r#"{"type":"alternator_extract_from_attrs","attr_name":"g","desired_type":"B"}"#
                    .to_string()
            )]
        );
        assert!(tags_of(gsi).contains_key(SPURIOUS_RANGE));
        let lsi = &p.keyspace.tables["t-1.x!:lsi"];
        assert!(tags_of(lsi).contains_key(SYNCHRONOUS));
        // Reading it back gives the table (transient fields aside).
        let back = load(&p.keyspace, "t-1.x").unwrap();
        assert_eq!(back.key, s.key);
        assert_eq!(back.gsis[0].key, s.gsis[0].key);
        assert_eq!(back.lsis[0].key, s.lsis[0].key);
        assert_eq!(back.billing_mode, "PROVISIONED");
        assert_eq!(back.provisioned, (5, 7));
        assert_eq!(back.ttl_attribute.as_deref(), Some("exp"));
        assert_eq!(back.tags, s.tags);
        assert_eq!(back.created_at_ms, s.created_at_ms);
        assert!(back.tablets);
        let st = back.stream.unwrap();
        assert_eq!((st.view_type.as_str(), st.enabled), ("OLD_IMAGE", true));
        assert!(
            load(&p.keyspace, "t-1.x:gsi").is_none(),
            "views aren't tables"
        );
    }

    #[test]
    fn changes_keep_ids_and_drop_removed_views() {
        let s = schema();
        let first = plan(&s, None, "datacenter1");
        let mut s2 = s.clone();
        s2.gsis.clear();
        s2.gsis.push(index(
            "other",
            IndexKind::Global,
            attr("l", ScalarType::S),
            None,
        ));
        s2.stream.as_mut().unwrap().enabled = false;
        let second = plan(&s2, Some(&first.keyspace), "datacenter1");
        assert_eq!(second.base.id, first.base.id);
        assert_eq!(second.drop, [first.keyspace.tables["t-1.x:gsi"].table_id()]);
        assert_eq!(second.build.len(), 1);
        assert!(
            second.keyspace.tables.contains_key("t-1.x_scylla_cdc_log"),
            "the log stays"
        );
        assert_eq!(second.base.params.scylla_cdc["enabled"], "false");
        let back = load(&second.keyspace, "t-1.x").unwrap();
        assert!(!back.stream.unwrap().enabled);
        assert_eq!(back.gsis.len(), 1);
    }

    #[test]
    fn vnodes_keyspaces_and_vector_indexes() {
        let mut s = schema();
        s.tablets = false;
        s.stream = None;
        s.vector_indexes.push(VectorIndex {
            name: "vix".into(),
            attribute: "v".into(),
            dimensions: 3,
            similarity: "COSINE".into(),
        });
        let p = plan(&s, None, "dc1");
        assert_eq!(p.keyspace.tablets, None);
        assert_eq!(p.keyspace.replication.options["dc1"], "1");
        let base = &p.keyspace.tables["t-1.x"];
        assert_eq!(base.indexes[0].options["class_name"], "vector_index");
        assert!(
            p.keyspace.tables.contains_key("t-1.x_scylla_cdc_log"),
            "vector search reads the log"
        );
        let back = load(&p.keyspace, "t-1.x").unwrap();
        assert_eq!(back.vector_indexes, s.vector_indexes);
        let st = back.stream.unwrap();
        assert!(!st.enabled, "a vector index's log is not a stream");
    }
}
