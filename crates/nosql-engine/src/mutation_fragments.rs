//! ScyllaDB's `SELECT ... FROM MUTATION_FRAGMENTS(t)` (`docs/operating-scylla/admin-tools/
//! select-from-mutation-fragments.rst`): the raw mutation fragments of a table, per data source.
//!
//! The statement reads a derived table, `t_$mutation_fragments`: the partition key of `t`, then
//! the clustering key `(mutation_source, partition_region, <t's clustering columns>,
//! position_weight)` and the regular columns `metadata`, `mutation_fragment_kind` and `value`.
//! Each partition of `t` gives, per source holding it, a `partition start` (region 0), its
//! `static row` (1), its `clustering row`s and `range tombstone change`s (2) in position order,
//! and a `partition end` (3). Positions that aren't a full clustering (partition start and end,
//! the static row, range tombstone bounds on a prefix) leave the missing components empty, as
//! ScyllaDB does (an `int` reads as null, a `text` as `''`).
//!
//! Sources, mapped onto roll's storage: `memtable:<n>` (0 is the active memtable), `row-cache`
//! (every flushed partition, as ScyllaDB's cache holds them after a flush or a read: the SSTables
//! merged), `sstable:<path>` per SSTable, and `virtual-table` for virtual tables.
//!
//! `metadata` and `value` are the JSON of `scylla sstable dump-data`, as probed on the reference:
//! cells `{"is_live", "type", "timestamp", "ttl", "expiry", "deletion_time"}`, collections
//! `{"tombstone", "cells": [{"key", "value"}]}`, row `tombstone` (with its `shadowable_tombstone`),
//! `marker` and `columns`; values print as ScyllaDB's `to_string` does (frozen collections as
//! `1, 2`, tuples and UDTs as `a:b`, timestamps in ISO form, blobs in hex).
//!
//! The rows are generated for the partitions the query restricts and filtered like a virtual
//! table's; paging resumes after the exact last row (the empty components don't sort as the
//! positions they stand for). ORDER BY is refused, as on ScyllaDB.

use std::fmt::Write as _;

use bytes::Bytes;
use nosql_storage::read::{ColumnFilter, Limits, Partition, PartitionRead, RangeRead, ReadOptions};
use nosql_storage::table::DataSource;
use nosql_storage::tombstones::Marker;
use nosql_storage::{Cell, Clustering, ColumnData, DeletionTime, LivenessInfo, PartitionKey, Row};
use nosql_types::{CqlType, Dialect};

use std::collections::{BTreeSet, HashSet};

use nosql_storage::Slice;
use nosql_storage::compare::{compare_bounds, compare_clustering_bound};

use crate::database::Database;
use crate::error::{Error, Result};
use crate::prepare::restrictions::Partitions;
use crate::schema::{Column, ColumnKind, Table, TableFlags, TableKind, TableParams};

/// The derived table's name suffix.
pub const SUFFIX: &str = "_$mutation_fragments";

/// Whether `t` is a `MUTATION_FRAGMENTS()` table.
pub fn is_derived(t: &Table) -> bool {
    t.kind == TableKind::Virtual && t.name.ends_with(SUFFIX)
}

/// ORDER BY is refused.
pub fn check_order_by(has_order_by: bool) -> Result<()> {
    if has_order_by {
        return Err(Error::invalid(
            "ORDER BY is not supported in SELECT FROM MUTATION_FRAGMENTS() statements.",
        ));
    }
    Ok(())
}

/// The derived table of `base` (its id and version: prepared statements follow the base).
pub fn derived_table(base: &Table) -> Table {
    let mut columns: Vec<Column> = Vec::new();
    let mut id = 0u32;
    let mut push = |name: &str, ty: CqlType, kind: ColumnKind, position: i32, reversed: bool| {
        columns.push(Column {
            name: name.to_string(),
            ty,
            kind,
            position,
            reversed,
            id,
        });
        id += 1;
    };
    for c in base.partition_key_columns() {
        push(
            &c.name,
            c.ty.clone(),
            ColumnKind::PartitionKey,
            c.position,
            false,
        );
    }
    push(
        "mutation_source",
        CqlType::Text,
        ColumnKind::Clustering,
        0,
        false,
    );
    push(
        "partition_region",
        CqlType::Tinyint,
        ColumnKind::Clustering,
        1,
        false,
    );
    let n = base.clustering.len() as i32;
    for c in base.clustering_columns() {
        push(
            &c.name,
            c.ty.clone(),
            ColumnKind::Clustering,
            c.position + 2,
            c.reversed,
        );
    }
    push(
        "position_weight",
        CqlType::Tinyint,
        ColumnKind::Clustering,
        n + 2,
        false,
    );
    for name in ["metadata", "mutation_fragment_kind", "value"] {
        push(name, CqlType::Text, ColumnKind::Regular, -1, false);
    }
    Table::new(
        base.keyspace.clone(),
        format!("{}{SUFFIX}", base.name),
        base.id,
        base.version,
        columns,
        TableParams::defaults(Dialect::Scylla, false),
        TableFlags {
            compound: true,
            dense: false,
            counter: false,
            super_: false,
        },
        TableKind::Virtual,
    )
}

/// A row cache dropped by REST `system/drop_sstable_caches`: it holds again the partitions read
/// since, and those of SSTables flushed since (a flush populates the cache); a range scan fills
/// it all.
#[derive(Debug, Default)]
pub struct Dropped {
    sstables: BTreeSet<String>,
    read: HashSet<Bytes>,
}

impl Database {
    /// Empties the row cache of every table (`POST /system/drop_sstable_caches`).
    pub fn drop_row_cache(&self) {
        let schema = self.schema();
        let mut m = self.live.row_cache.lock();
        for k in schema.user_keyspaces() {
            for t in k.tables.values() {
                let Ok(st) = self.storage_table(t) else {
                    continue;
                };
                let sstables = st
                    .sources()
                    .into_iter()
                    .filter_map(|(s, _)| match s {
                        DataSource::SsTable(key) => Some(key),
                        DataSource::Memtable(_) => None,
                    })
                    .collect();
                m.insert(
                    t.id,
                    Dropped {
                        sstables,
                        read: HashSet::new(),
                    },
                );
            }
        }
    }
}

/// A read of `t` (it fills the row cache).
pub(crate) fn note_read(db: &Database, t: &Table, parts: &Partitions) {
    let mut m = db.live.row_cache.lock();
    if m.is_empty() {
        return;
    }
    match parts {
        Partitions::Keys(keys) => {
            if let Some(d) = m.get_mut(&t.id) {
                for k in keys {
                    if let Ok(pk) = crate::storage::partition_key(k, db.dialect(), false) {
                        d.read.insert(pk.key);
                    }
                }
            }
        }
        Partitions::Range { .. } => {
            m.remove(&t.id);
        }
    }
}

/// A source's name and its partitions (raw: tombstones included).
type SourceData = (String, Vec<Partition>);

fn raw(now: i64) -> ReadOptions {
    ReadOptions {
        raw: true,
        ..ReadOptions::at(now)
    }
}

/// The SSTable's name as ScyllaDB's `sstable:` sources show a path.
fn sstable_path(base: &Table, key: &str) -> String {
    let id: String = base.id.iter().map(|b| format!("{b:02x}")).collect();
    let file = key.rsplit('/').next().unwrap_or(key);
    format!(
        "sstable:/var/lib/scylla/data/{}/{}-{id}/{file}-big-Data.db",
        base.keyspace, base.name
    )
}

async fn collect(stream: nosql_storage::read::ReadStream) -> Result<Vec<Partition>> {
    stream
        .partitions()
        .await
        .map_err(|e| Error::server(e.to_string()))
}

/// The partitions of every source of a stored table, restricted to `parts`.
async fn stored_sources(
    db: &Database,
    base: &Table,
    parts: &Partitions,
    keys: &[PartitionKey],
    now: i64,
) -> Result<Vec<SourceData>> {
    let st = db.storage_table(base)?;
    let read = |state: &nosql_storage::table::TableState| {
        let state = state.clone();
        let st = st.clone();
        async move {
            match parts {
                Partitions::Keys(_) => {
                    let mut out = Vec::new();
                    for k in keys {
                        let r = PartitionRead {
                            key: k.clone(),
                            clustering: nosql_storage::read::ClusteringFilter::all(),
                            reversed: false,
                            columns: ColumnFilter::all(),
                            limits: Limits::default(),
                            options: raw(now),
                        };
                        out.extend(collect(st.read_from(&state, r)).await?);
                    }
                    Ok::<_, Error>(out)
                }
                Partitions::Range { start, end } => {
                    let r = RangeRead {
                        start: start.clone(),
                        end: end.clone(),
                        options: raw(now),
                        ..RangeRead::all(now)
                    };
                    collect(st.scan_from(&state, r)).await
                }
            }
        }
    };
    let mut out: Vec<SourceData> = Vec::new();
    let sources = st.sources();
    let mut sstables = 0;
    // Partitions of SSTables flushed since the row cache was dropped.
    let mut fresh: HashSet<Bytes> = HashSet::new();
    let dropped: Option<(BTreeSet<String>, HashSet<Bytes>)> = db
        .live
        .row_cache
        .lock()
        .get(&base.id)
        .map(|d| (d.sstables.clone(), d.read.clone()));
    for (source, state) in &sources {
        let name = match source {
            DataSource::Memtable(i) => format!("memtable:{i}"),
            DataSource::SsTable(key) => {
                sstables += 1;
                sstable_path(base, key)
            }
        };
        let parts = read(state).await?;
        if let (DataSource::SsTable(key), Some((old, _))) = (source, &dropped)
            && !old.contains(key)
        {
            fresh.extend(parts.iter().map(|p| p.key.key.clone()));
        }
        if !parts.is_empty() {
            out.push((name, parts));
        }
    }
    if sstables > 0 {
        let mut parts = read(&st.flushed()).await?;
        if let Some((_, read)) = &dropped {
            parts.retain(|p| read.contains(&p.key.key) || fresh.contains(&p.key.key));
        }
        if !parts.is_empty() {
            out.push(("row-cache".to_string(), parts));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// The derived rows of `base`'s partitions: one entry per partition key, in key order for
/// `Partitions::Keys` (`keys`) and token order otherwise. `virtual_parts`: a virtual table's
/// generated partitions (its only source).
#[allow(clippy::too_many_arguments)]
pub async fn partitions(
    db: &Database,
    t: &Table,
    base: &Table,
    parts: &Partitions,
    keys: &[PartitionKey],
    virtual_parts: Option<Vec<Partition>>,
    clip: Option<&[(Bytes, Slice)]>,
    now: i64,
) -> Result<Vec<(PartitionKey, Vec<Row>)>> {
    let sources: Vec<SourceData> = match virtual_parts {
        Some(p) => vec![("virtual-table".to_string(), p)],
        None => stored_sources(db, base, parts, keys, now).await?,
    };
    let mut order: Vec<PartitionKey> = match parts {
        Partitions::Keys(_) => keys.to_vec(),
        Partitions::Range { .. } => {
            let mut all: Vec<PartitionKey> = sources
                .iter()
                .flat_map(|(_, ps)| ps.iter().map(|p| p.key.clone()))
                .collect();
            all.sort();
            all.dedup();
            all
        }
    };
    order.dedup();
    let fmt = Format {
        base,
        t,
        now,
        host: db.config().host_id(),
        virtual_ts: db.timestamp(),
        is_virtual: base.kind != TableKind::Regular,
    };
    let mut out = Vec::with_capacity(order.len());
    for key in order {
        let mut rows = Vec::new();
        for (name, ps) in &sources {
            for p in ps.iter().filter(|p| p.key == key) {
                match clip {
                    None => fmt.partition(name, p, &mut rows),
                    Some(slices) => {
                        for (src, slice) in slices {
                            if src[..] == *name.as_bytes() {
                                fmt.clipped(name, p, slice, &mut rows);
                            }
                        }
                    }
                }
            }
        }
        if !rows.is_empty() {
            out.push((key, rows));
        }
    }
    Ok(out)
}

struct Format<'a> {
    base: &'a Table,
    t: &'a Table,
    now: i64,
    host: [u8; 16],
    /// Virtual tables' cells carry the read's time as their timestamp.
    virtual_ts: i64,
    is_virtual: bool,
}

const START: u8 = 0;
const STATIC: u8 = 1;
const CLUSTERED: u8 = 2;
const END: u8 = 3;

impl Format<'_> {
    fn partition(&self, source: &str, p: &Partition, out: &mut Vec<Row>) {
        let start = format!("{{\"tombstone\":{}}}", tombstone(&p.partition_deletion));
        out.push(self.row(
            source,
            START,
            None,
            None,
            Some(start),
            "partition start",
            None,
            0,
        ));
        if let Some(s) = &p.static_row {
            let (meta, value, ts) = self.columns(s, true);
            out.push(self.row(
                source,
                STATIC,
                None,
                None,
                Some(meta),
                "static row",
                Some(value),
                ts,
            ));
        }
        // Rows and range tombstone changes, in position order (markers sit between rows).
        let cmp = &*self.base.comparator;
        let mut markers = p.markers.iter().peekable();
        for r in &p.rows {
            while let Some(m) = markers.next_if(|m| marker_before(cmp, m, &r.clustering)) {
                out.push(self.marker(source, m));
            }
            out.push(self.clustering_row(source, r));
        }
        for m in markers {
            out.push(self.marker(source, m));
        }
        out.push(self.row(source, END, None, None, None, "partition end", None, 0));
    }

    /// The clustered fragments of `p` within `slice`, range tombstones cut at its bounds (as a
    /// sliced read of ScyllaDB returns them).
    fn clipped(&self, source: &str, p: &Partition, slice: &Slice, out: &mut Vec<Row>) {
        let cmp = &*self.base.comparator;
        let inside = |b: &nosql_storage::Bound| {
            compare_bounds(cmp, b, &slice.start).is_ge()
                && compare_bounds(cmp, b, &slice.end).is_lt()
        };
        let mut current = DeletionTime::LIVE;
        for m in p
            .markers
            .iter()
            .filter(|m| compare_bounds(cmp, &m.bound, &slice.start).is_lt())
        {
            current = m.open.unwrap_or(DeletionTime::LIVE);
        }
        let change = |b: &nosql_storage::Bound, t: &DeletionTime| {
            self.row(
                source,
                CLUSTERED,
                Some(&b.prefix),
                Some(if b.after { 1 } else { -1 }),
                Some(format!("{{\"tombstone\":{}}}", tombstone(t))),
                "range tombstone change",
                None,
                0,
            )
        };
        if !current.is_live() {
            out.push(change(&slice.start, &current));
        }
        let mut markers = p.markers.iter().filter(|m| inside(&m.bound)).peekable();
        let rows = p.rows.iter().filter(|r| {
            compare_clustering_bound(cmp, &r.clustering, &slice.start).is_gt()
                && compare_clustering_bound(cmp, &r.clustering, &slice.end).is_lt()
        });
        for r in rows {
            while let Some(m) = markers.next_if(|m| marker_before(cmp, m, &r.clustering)) {
                current = m.open.unwrap_or(DeletionTime::LIVE);
                out.push(self.marker(source, m));
            }
            out.push(self.clustering_row(source, r));
        }
        for m in markers {
            current = m.open.unwrap_or(DeletionTime::LIVE);
            out.push(self.marker(source, m));
        }
        if !current.is_live() {
            out.push(change(&slice.end, &DeletionTime::LIVE));
        }
    }

    /// A derived row at `(source, region, clustering, weight)`.
    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        source: &str,
        region: u8,
        clustering: Option<&Clustering>,
        weight: Option<i8>,
        metadata: Option<String>,
        kind: &str,
        value: Option<String>,
        ts: i64,
    ) -> Row {
        let mut parts: Vec<Bytes> = vec![
            Bytes::copy_from_slice(source.as_bytes()),
            Bytes::copy_from_slice(&[region]),
        ];
        for i in 0..self.base.clustering.len() {
            parts.push(clustering.and_then(|c| c.get(i)).unwrap_or_default());
        }
        parts.push(weight.map_or_else(Bytes::new, |w| Bytes::copy_from_slice(&[w as u8])));
        let id = |n: &str| self.t.column(n).map_or(0, |c| c.id);
        let mut row = Row::new(Clustering::new(parts)).with_liveness(LivenessInfo::live(0));
        let mut cells: Vec<(u32, Cell)> = vec![(
            id("mutation_fragment_kind"),
            Cell::live(0, Bytes::copy_from_slice(kind.as_bytes())),
        )];
        if let Some(m) = metadata {
            cells.push((id("metadata"), Cell::live(0, Bytes::from(m))));
        }
        if let Some(v) = value {
            cells.push((id("value"), Cell::live(ts, Bytes::from(v))));
        }
        cells.sort_by_key(|(id, _)| *id);
        for (id, c) in cells {
            row = row.with_cell(id, c);
        }
        row
    }

    fn marker(&self, source: &str, m: &Marker) -> Row {
        let weight = if m.bound.after { 1 } else { -1 };
        let t = m.open.unwrap_or(DeletionTime::LIVE);
        self.row(
            source,
            CLUSTERED,
            Some(&m.bound.prefix),
            Some(weight),
            Some(format!("{{\"tombstone\":{}}}", tombstone(&t))),
            "range tombstone change",
            None,
            0,
        )
    }

    fn clustering_row(&self, source: &str, r: &Row) -> Row {
        let mut meta = String::from("{");
        if !r.deletion.time.is_live() {
            let t = tombstone(&r.deletion.time);
            if r.deletion.shadowable {
                let _ = write!(meta, "\"shadowable_tombstone\":{t},");
            } else {
                let _ = write!(meta, "\"tombstone\":{t},\"shadowable_tombstone\":{t},");
            }
        }
        if !r.liveness.is_empty() && !self.is_virtual {
            let l = &r.liveness;
            let _ = write!(meta, "\"marker\":{{\"timestamp\":{}", l.timestamp);
            if l.ttl > 0 {
                let _ = write!(
                    meta,
                    ",\"ttl\":\"{}s\",\"expiry\":\"{}\"",
                    l.ttl,
                    date(l.local_expiration_time)
                );
            }
            meta.push_str("},");
        }
        let (cols, value, ts) = self.columns(r, false);
        let _ = write!(meta, "\"columns\":{cols}}}");
        self.row(
            source,
            CLUSTERED,
            Some(&r.clustering),
            Some(0),
            Some(meta),
            "clustering row",
            Some(value),
            ts,
        )
    }

    /// `$COLUMNS` of a row and its values; the newest timestamp.
    fn columns(&self, r: &Row, statics: bool) -> (String, String, i64) {
        let mut cols: Vec<&Column> = self
            .base
            .columns
            .iter()
            .filter(|c| {
                if statics {
                    c.kind == ColumnKind::Static
                } else {
                    c.kind == ColumnKind::Regular
                }
            })
            .collect();
        cols.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let (mut meta, mut value) = (Vec::new(), Vec::new());
        let mut newest = i64::MIN;
        for c in cols {
            let Some(data) = r.column(c.id) else { continue };
            let name = json_str(&c.name);
            match data {
                ColumnData::Simple { cell, .. } => {
                    newest = newest.max(cell.timestamp);
                    let kind = if cell.is_counter() {
                        "counter-shards"
                    } else if matches!(
                        c.ty,
                        CqlType::List { .. } | CqlType::Set { .. } | CqlType::Map { .. }
                    ) {
                        "frozen-collection"
                    } else {
                        "regular"
                    };
                    meta.push(format!("{name}:{}", self.cell(cell, kind)));
                    value.push(format!("{name}:{}", self.simple_value(&c.ty, cell)));
                }
                ColumnData::Complex {
                    deletion, cells, ..
                } => {
                    let mut m = String::from("{");
                    if !deletion.is_live() {
                        let _ = write!(m, "\"tombstone\":{},", tombstone(deletion));
                    }
                    let mut entries = Vec::new();
                    let mut values = Vec::new();
                    for cell in cells {
                        newest = newest.max(cell.timestamp);
                        let key = json_str(&path_text(&c.ty, cell.path.as_deref()));
                        entries.push(format!(
                            "{{\"key\":{key},\"value\":{}}}",
                            self.cell(cell, "regular")
                        ));
                        let v = if cell.is_live(self.now) {
                            match &c.ty {
                                CqlType::Set { .. } => json_str(""),
                                ty => json_str(&to_string(element_type(ty), &cell.value)),
                            }
                        } else {
                            "null".to_string()
                        };
                        values.push(format!("{{\"key\":{key},\"value\":{v}}}"));
                    }
                    let _ = write!(m, "\"cells\":[{}]}}", entries.join(","));
                    meta.push(format!("{name}:{m}"));
                    value.push(format!("{name}:[{}]", values.join(",")));
                }
            }
        }
        (
            format!("{{{}}}", meta.join(",")),
            format!("{{{}}}", value.join(",")),
            if newest == i64::MIN { 0 } else { newest },
        )
    }

    /// `$REGULAR_CELL` (and its counter and frozen collection forms), without the value.
    fn cell(&self, c: &Cell, kind: &str) -> String {
        let ts = if self.is_virtual {
            self.virtual_ts
        } else {
            c.timestamp
        };
        let live = c.is_live(self.now);
        let mut s = format!("{{\"is_live\":{live},\"type\":\"{kind}\",\"timestamp\":{ts}");
        if c.is_expiring() {
            let _ = write!(
                s,
                ",\"ttl\":\"{}s\",\"expiry\":\"{}\"",
                c.ttl,
                date(c.local_deletion_time)
            );
        } else if c.is_tombstone() {
            let _ = write!(s, ",\"deletion_time\":\"{}\"", date(c.local_deletion_time));
        }
        s.push('}');
        s
    }

    fn simple_value(&self, ty: &CqlType, c: &Cell) -> String {
        if !c.is_live(self.now) {
            return "null".to_string();
        }
        if let Some((clock, total)) = c.counter_parts() {
            return format!(
                "[{{\"id\":\"{}\",\"value\":{total},\"clock\":{clock}}}]",
                crate::uuid::format(&self.host)
            );
        }
        json_str(&to_string(ty, &c.value))
    }
}

/// The base slices of a derived clustering filter that fixes the source and the clustered region
/// of every slice (`None` otherwise: the rows are filtered as they are).
pub fn base_slices(
    t: &Table,
    base: &Table,
    f: &nosql_storage::read::ClusteringFilter,
) -> Option<Vec<(Bytes, Slice)>> {
    let nosql_storage::read::ClusteringFilter::Slices(slices) = f else {
        return None;
    };
    let n = base.clustering.len();
    if n == 0 || t.clustering.len() != n + 3 {
        return None;
    }
    let mut out = Vec::new();
    for s in slices {
        let (a, b) = (&s.start.prefix, &s.end.prefix);
        if a.len() < 2 || b.len() < 2 || a.prefix(2) != b.prefix(2) {
            return None;
        }
        if a.get(1).as_deref() != Some(&[CLUSTERED][..]) {
            return None;
        }
        let strip = |c: &Clustering| Clustering::new(c.components().skip(2).take(n));
        let bound = |b: &nosql_storage::Bound| nosql_storage::Bound {
            prefix: strip(&b.prefix),
            after: b.after,
        };
        out.push((
            a.get(0).unwrap_or_default(),
            Slice::new(bound(&s.start), bound(&s.end)),
        ));
    }
    Some(out)
}

/// Whether range tombstone marker `m` comes before the row at `c`.
fn marker_before(
    cmp: &dyn nosql_storage::ClusteringComparator,
    m: &Marker,
    c: &Clustering,
) -> bool {
    nosql_storage::compare::compare_clustering_bound(cmp, c, &m.bound).is_gt()
}

/// `$TOMBSTONE` (`{}` when live).
fn tombstone(d: &DeletionTime) -> String {
    if d.is_live() {
        return "{}".to_string();
    }
    format!(
        "{{\"timestamp\":{},\"deletion_time\":\"{}\"}}",
        d.marked_at,
        date(d.local_deletion_time)
    )
}

/// A deletion or expiry time as dump-data prints it: `2026-10-21 04:08:22z`.
fn date(secs: i64) -> String {
    let s =
        nosql_types::temporal::format_timestamp(secs.saturating_mul(1000), ' ', Dialect::Scylla);
    format!("{}z", s.strip_suffix(".000Z").unwrap_or(&s))
}

/// The element type a complex column's cells hold.
fn element_type(ty: &CqlType) -> &CqlType {
    match ty {
        CqlType::List { elem, .. } | CqlType::Set { elem, .. } => elem,
        CqlType::Map { value, .. } => value,
        _ => ty,
    }
}

/// A cell path as dump-data names it: the list's timeuuid, the set element, the map key; UDT
/// fields print empty.
fn path_text(ty: &CqlType, path: Option<&[u8]>) -> String {
    let Some(p) = path else {
        return String::new();
    };
    match ty {
        CqlType::List { .. } => to_string(&CqlType::Timeuuid, p),
        CqlType::Set { elem, .. } => to_string(elem, p),
        CqlType::Map { key, .. } => to_string(key, p),
        _ => String::new(),
    }
}

/// ScyllaDB's `to_string` of a value.
fn to_string(ty: &CqlType, b: &[u8]) -> String {
    match ty {
        CqlType::Text | CqlType::Ascii => String::from_utf8_lossy(b).into_owned(),
        CqlType::Blob | CqlType::Custom(_) => b.iter().map(|x| format!("{x:02x}")).collect(),
        CqlType::Timestamp => match <[u8; 8]>::try_from(b) {
            Ok(a) => {
                nosql_types::temporal::format_timestamp(i64::from_be_bytes(a), 'T', Dialect::Scylla)
            }
            Err(_) => String::new(),
        },
        CqlType::List { elem, .. } | CqlType::Set { elem, .. } => {
            nosql_types::collection_elements(b)
                .unwrap_or_default()
                .into_iter()
                .map(|e| e.map(|e| to_string(elem, e)).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(", ")
        }
        CqlType::Map { key, value, .. } => nosql_types::map_entries(b)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| format!("{}:{}", to_string(key, k), to_string(value, v)))
            .collect::<Vec<_>>()
            .join(", "),
        CqlType::Tuple(types) => fields(types.iter(), b),
        CqlType::Udt(def, _) => fields(def.fields.iter().map(|(_, t)| t), b),
        _ => nosql_types::to_text(ty, b, Dialect::Scylla)
            .ok()
            .flatten()
            .unwrap_or_default(),
    }
}

fn fields<'a>(types: impl Iterator<Item = &'a CqlType>, b: &[u8]) -> String {
    let values = nosql_types::tuple_fields(b).unwrap_or_default();
    types
        .zip(values)
        .map(|(t, v)| v.map(|v| to_string(t, v)).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(":")
}

/// A JSON string literal.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_strings() {
        assert_eq!(date(1_792_555_702), "2026-10-21 04:08:22z");
        assert_eq!(json_str("it's \"q\""), r#""it's \"q\"""#);
        assert_eq!(
            to_string(
                &CqlType::List {
                    elem: std::sync::Arc::new(CqlType::Int),
                    frozen: true
                },
                &nosql_types::pack_collection([&1i32.to_be_bytes()[..], &2i32.to_be_bytes()[..]])
            ),
            "1, 2"
        );
        assert_eq!(to_string(&CqlType::Blob, &[0xca, 0xfe]), "cafe");
    }
}
