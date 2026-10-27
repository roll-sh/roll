//! How a catalog is stored (catalog format 1): a header record under `txn::CATALOG_KEY`
//! holding the catalog's scalar fields and lists, and one record per entry of each of its
//! per-object maps and sets, under `RECORD_PREFIX`, a tag naming the map and the entry's oid.
//!
//! A commit of DDL writes only the records whose entries changed (`diff`: maps of `Arc`s
//! compare by pointer, which a DDL statement's copy of the catalog shares for every object it
//! didn't touch; other maps by value), so its cost follows the objects it changed, not the
//! catalog's size. The header and the records go in the commit's one WAL record, so they are
//! replaced together, and a crash leaves either version whole.
//!
//! Before it (format 0, `catalog_format` absent) the whole catalog was one JSON value under
//! `CATALOG_KEY`; `Database::open` loads such a catalog and rewrites it in this format in one
//! commit.

use super::Catalog;
use super::pmap::PMap;
use crate::error::{Result, internal};
use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The catalog format this build writes (`Catalog::catalog_format`).
pub const CATALOG_FORMAT: u32 = 2;

/// Where the per-object records lie: after `CATALOG_KEY` and the sequences' keys, in the
/// reserved keyspace of table oid 0.
pub const RECORD_PREFIX: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 3];
const RECORD_END: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 4];

/// A record's key: the prefix, the map's tag, the entry's oid.
fn record_key(tag: u8, oid: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(RECORD_PREFIX.len() + 5);
    k.extend_from_slice(RECORD_PREFIX);
    k.push(tag);
    k.extend_from_slice(&oid.to_be_bytes());
    k
}

/// Whether `key` is one of the catalog's keys (the header or a record).
pub fn is_catalog_key(key: &[u8]) -> bool {
    key == crate::txn::CATALOG_KEY || key.starts_with(RECORD_PREFIX)
}

/// The key range of the records.
pub fn record_range() -> (&'static [u8], &'static [u8]) {
    (RECORD_PREFIX, RECORD_END)
}

type Writes = Vec<(Vec<u8>, Option<Bytes>)>;

fn json<T: Serialize + ?Sized>(v: &T) -> Result<Bytes> {
    serde_json::to_vec(v)
        .map(Bytes::from)
        .map_err(|e| internal(e.to_string()))
}

/// The writes turning map `a`'s records into map `b`'s: entries `same` says are unchanged
/// are skipped.
fn diff_map<V: Serialize>(
    tag: u8,
    a: &BTreeMap<u32, V>,
    b: &BTreeMap<u32, V>,
    same: fn(&V, &V) -> bool,
    out: &mut Writes,
) -> Result<()> {
    let mut ia = a.iter().peekable();
    let mut ib = b.iter().peekable();
    loop {
        match (ia.peek(), ib.peek()) {
            (None, None) => return Ok(()),
            (Some((ka, _)), None) => {
                out.push((record_key(tag, **ka), None));
                ia.next();
            }
            (None, Some((kb, vb))) => {
                out.push((record_key(tag, **kb), Some(json(vb)?)));
                ib.next();
            }
            (Some((ka, va)), Some((kb, vb))) => {
                if ka < kb {
                    out.push((record_key(tag, **ka), None));
                    ia.next();
                } else if kb < ka {
                    out.push((record_key(tag, **kb), Some(json(vb)?)));
                    ib.next();
                } else {
                    if !same(va, vb) {
                        out.push((record_key(tag, **kb), Some(json(vb)?)));
                    }
                    ia.next();
                    ib.next();
                }
            }
        }
    }
}

fn diff_set(tag: u8, a: &BTreeSet<u32>, b: &BTreeSet<u32>, out: &mut Writes) {
    for x in a.difference(b) {
        out.push((record_key(tag, *x), None));
    }
    for x in b.difference(a) {
        out.push((record_key(tag, *x), Some(Bytes::new())));
    }
}

fn put_set(tag: u8, s: &BTreeSet<u32>, out: &mut Writes) {
    for x in s {
        out.push((record_key(tag, *x), Some(Bytes::new())));
    }
}

fn parse<V: DeserializeOwned>(v: &[u8]) -> Result<V> {
    serde_json::from_slice(v).map_err(|e| internal(format!("corrupt catalog: {e}")))
}

/// Records parsed per thread at least (fewer aren't worth a thread).
const PARSE_CHUNK: usize = 256;

/// `items`' values parsed, in order: on several threads when there are many (a database's
/// first open reads every record; parsing them is most of it).
fn parse_all<V: DeserializeOwned + Send>(items: &[(u32, &[u8])]) -> Result<Vec<(u32, V)>> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(8)
        .min(items.len() / PARSE_CHUNK)
        .max(1);
    let one = |part: &[(u32, &[u8])]| -> Result<Vec<(u32, V)>> {
        part.iter().map(|(oid, v)| Ok((*oid, parse(v)?))).collect()
    };
    if threads == 1 {
        return one(items);
    }
    let size = items.len().div_ceil(threads);
    std::thread::scope(|s| {
        let parts: Vec<_> = items.chunks(size).map(|part| s.spawn(move || one(part))).collect();
        let mut out = Vec::with_capacity(items.len());
        for p in parts {
            out.extend(p.join().map_err(|_| internal("catalog parse panicked"))??);
        }
        Ok(out)
    })
}

/// A stored map: a plain one, or a `PMap`, whose copies share chunks.
trait StoredMap<V> {
    /// The writes turning `self`'s records into `new`'s.
    fn diff_into(&self, tag: u8, new: &Self, same: fn(&V, &V) -> bool, out: &mut Writes) -> Result<()>;
    fn put_all(&self, tag: u8, out: &mut Writes) -> Result<()>;
    fn load_one(&mut self, oid: u32, v: V);
    fn remove_one(&mut self, oid: u32);
    fn get_one(&self, oid: u32) -> Option<&V>;
    /// The keys whose entries differ in `new` (no record made).
    fn changed_keys(&self, new: &Self, same: fn(&V, &V) -> bool, out: &mut Vec<u32>);
}

impl<V: Serialize> StoredMap<V> for BTreeMap<u32, V> {
    fn diff_into(&self, tag: u8, new: &Self, same: fn(&V, &V) -> bool, out: &mut Writes) -> Result<()> {
        diff_map(tag, self, new, same, out)
    }
    fn put_all(&self, tag: u8, out: &mut Writes) -> Result<()> {
        for (k, v) in self {
            out.push((record_key(tag, *k), Some(json(v)?)));
        }
        Ok(())
    }
    fn load_one(&mut self, oid: u32, v: V) {
        self.insert(oid, v);
    }
    fn remove_one(&mut self, oid: u32) {
        self.remove(&oid);
    }
    fn get_one(&self, oid: u32) -> Option<&V> {
        self.get(&oid)
    }
    fn changed_keys(&self, new: &Self, same: fn(&V, &V) -> bool, out: &mut Vec<u32>) {
        for (k, v) in self {
            if new.get(k).is_none_or(|n| !same(v, n)) {
                out.push(*k);
            }
        }
        out.extend(new.keys().filter(|k| !self.contains_key(k)));
    }
}

/// (Chunks the two maps share are skipped: a statement's diff follows what it changed.)
impl<V: Serialize + Clone> StoredMap<V> for PMap<V> {
    fn diff_into(&self, tag: u8, new: &Self, same: fn(&V, &V) -> bool, out: &mut Writes) -> Result<()> {
        let mut changed = Vec::new();
        self.diff(new, same, |k, _, b| changed.push((k, b)));
        for (k, b) in changed {
            out.push((record_key(tag, k), b.map(json).transpose()?));
        }
        Ok(())
    }
    fn put_all(&self, tag: u8, out: &mut Writes) -> Result<()> {
        for (k, v) in self {
            out.push((record_key(tag, *k), Some(json(v)?)));
        }
        Ok(())
    }
    fn load_one(&mut self, oid: u32, v: V) {
        self.insert(oid, v);
    }
    fn remove_one(&mut self, oid: u32) {
        self.remove(&oid);
    }
    fn get_one(&self, oid: u32) -> Option<&V> {
        self.get(&oid)
    }
    fn changed_keys(&self, new: &Self, same: fn(&V, &V) -> bool, out: &mut Vec<u32>) {
        self.diff(new, same, |k, _, _| out.push(k));
    }
}

impl<V: Serialize + Clone, I: super::pmap::MapIndex<V>> StoredMap<V> for super::pmap::IndexedMap<V, I> {
    fn diff_into(&self, tag: u8, new: &Self, same: fn(&V, &V) -> bool, out: &mut Writes) -> Result<()> {
        let (a, b): (&PMap<V>, &PMap<V>) = (self, new);
        a.diff_into(tag, b, same, out)
    }
    fn put_all(&self, tag: u8, out: &mut Writes) -> Result<()> {
        let a: &PMap<V> = self;
        a.put_all(tag, out)
    }
    fn load_one(&mut self, oid: u32, v: V) {
        self.insert(oid, v);
    }
    fn remove_one(&mut self, oid: u32) {
        self.remove(&oid);
    }
    fn get_one(&self, oid: u32) -> Option<&V> {
        self.get(&oid)
    }
    fn changed_keys(&self, new: &Self, same: fn(&V, &V) -> bool, out: &mut Vec<u32>) {
        let (a, b): (&PMap<V>, &PMap<V>) = (self, new);
        a.changed_keys(b, same, out)
    }
}

/// One tag's parsed records (a `Vec<(u32, V)>` of its map's values, or a set's oids).
type Parsed = Box<dyn std::any::Any + Send>;

fn parse_for<V: DeserializeOwned + Send + 'static, M: StoredMap<V>>(_: &M, items: &[(u32, &[u8])]) -> Result<Parsed> {
    Ok(Box::new(parse_all::<V>(items)?))
}

fn insert_from<V: 'static, M: StoredMap<V>>(m: &mut M, p: Parsed) -> Result<()> {
    let v = p.downcast::<Vec<(u32, V)>>().map_err(|_| internal("catalog map records"))?;
    for (oid, x) in *v {
        m.load_one(oid, x);
    }
    Ok(())
}

fn by_ptr<T>(a: &Arc<T>, b: &Arc<T>) -> bool {
    Arc::ptr_eq(a, b)
}

fn by_value<T: PartialEq>(a: &T, b: &T) -> bool {
    a == b
}

/// Every field of `Catalog`, by how it is stored: `maps` (a record per entry: the tag, and
/// whether entries compare by pointer or by value), `sets` (a record per member) and
/// `header` (in the header record). The destructuring in `fields_are_all_listed` is
/// exhaustive, so a field added to `Catalog` without a place here doesn't compile (it would
/// silently not be stored).
macro_rules! stored_fields {
    (
        maps: [$($m:ident = $mtag:literal $cmp:ident),* $(,)?],
        sets: [$($s:ident = $stag:literal),* $(,)?],
        header: [$($h:ident),* $(,)?],
        derived: [$($d:ident),* $(,)?] $(,)?
    ) => {
        #[allow(dead_code)]
        fn fields_are_all_listed(c: &Catalog) {
            let Catalog { $($m: _,)* $($s: _,)* $($h: _,)* $($d: _,)* } = c;
        }

        /// The records that change catalog `a` into `b`.
        fn diff_records(a: &Catalog, b: &Catalog, out: &mut Writes) -> Result<()> {
            $(a.$m.diff_into($mtag, &b.$m, $cmp, out)?;)*
            $(diff_set($stag, &a.$s, &b.$s, out);)*
            Ok(())
        }

        /// Every record of `c`.
        fn all_records(c: &Catalog, out: &mut Writes) -> Result<()> {
            $(c.$m.put_all($mtag, out)?;)*
            $(put_set($stag, &c.$s, out);)*
            Ok(())
        }

        fn header(c: &Catalog) -> Result<Bytes> {
            json(&header_map(c)?)
        }

        /// The (tag, oid) of the records that differ between `a` and `b`, without making
        /// them (a merge of concurrent DDL copies entries, `copy_one`).
        fn changed_records(a: &Catalog, b: &Catalog, out: &mut Vec<(u8, u32)>) {
            let mut keys = Vec::new();
            $(
                keys.clear();
                a.$m.changed_keys(&b.$m, $cmp, &mut keys);
                out.extend(keys.iter().map(|k| ($mtag, *k)));
            )*
            $(
                out.extend(a.$s.symmetric_difference(&b.$s).map(|k| ($stag, *k)));
            )*
        }

        /// Whether record (`tag`, `oid`) is alike in `a` and `b`.
        fn same_one(a: &Catalog, b: &Catalog, tag: u8, oid: u32) -> bool {
            match tag {
                $($mtag => match (a.$m.get_one(oid), b.$m.get_one(oid)) {
                    (None, None) => true,
                    (Some(x), Some(y)) => $cmp(x, y),
                    _ => false,
                },)*
                $($stag => a.$s.contains(&oid) == b.$s.contains(&oid),)*
                _ => false,
            }
        }

        /// Record (`tag`, `oid`) of `from` put in `to` (its entry, or none).
        fn copy_one(from: &Catalog, to: &mut Catalog, tag: u8, oid: u32) {
            match tag {
                $($mtag => match from.$m.get_one(oid) {
                    Some(v) => to.$m.load_one(oid, v.clone()),
                    None => to.$m.remove_one(oid),
                },)*
                $($stag => {
                    if from.$s.contains(&oid) {
                        to.$s.insert(oid);
                    } else {
                        to.$s.remove(&oid);
                    }
                })*
                _ => {}
            }
        }

        /// Record (`tag`, `oid`) of `c` as JSON (None: no entry; a set's member is null).
        fn one_json(c: &Catalog, tag: u8, oid: u32) -> Result<Option<serde_json::Value>> {
            match tag {
                $($mtag => c.$m.get_one(oid)
                    .map(|x| serde_json::to_value(x).map_err(|e| internal(e.to_string())))
                    .transpose(),)*
                $($stag => Ok(c.$s.contains(&oid).then_some(serde_json::Value::Null)),)*
                _ => Ok(None),
            }
        }

        /// Every record tag, ascending.
        fn all_tags() -> Vec<u8> {
            let mut t = vec![$($mtag,)* $($stag,)*];
            t.sort_unstable();
            t
        }

        /// One map's (or set's) records, in oid order, parsed (`parse_all`; on whichever
        /// thread loads that tag, `load`).
        fn parse_tag(c: &Catalog, tag: u8, items: &[(u32, &[u8])]) -> Result<Parsed> {
            match tag {
                $($mtag => parse_for(&c.$m, items),)*
                $($stag => Ok(Box::new(items.iter().map(|(oid, _)| *oid).collect::<Vec<u32>>())),)*
                _ => Err(internal(format!("corrupt catalog: unknown record tag {tag}"))),
            }
        }

        /// `parse_tag`'s values inserted in order.
        fn insert_tag(c: &mut Catalog, tag: u8, p: Parsed) -> Result<()> {
            match tag {
                $($mtag => insert_from(&mut c.$m, p),)*
                $($stag => {
                    let v = p.downcast::<Vec<u32>>().map_err(|_| internal("catalog set records"))?;
                    c.$s.extend(v.into_iter());
                    Ok(())
                })*
                _ => Err(internal(format!("corrupt catalog: unknown record tag {tag}"))),
            }
        }

        /// The header's fields as JSON (with `catalog_format`).
        fn header_map(c: &Catalog) -> Result<serde_json::Map<String, serde_json::Value>> {
            let mut m = serde_json::Map::new();
            m.insert("catalog_format".into(), CATALOG_FORMAT.into());
            $(m.insert(
                stringify!($h).into(),
                serde_json::to_value(&c.$h).map_err(|e| internal(e.to_string()))?,
            );)*
            Ok(m)
        }

        /// The header fields in which `a` and `b` differ (compared as values, not as JSON).
        fn header_diff(a: &Catalog, b: &Catalog) -> Vec<&'static str> {
            let mut out = Vec::new();
            $(if a.$h != b.$h {
                out.push(stringify!($h));
            })*
            out
        }

        /// Header field `name` as JSON (None: not a header field).
        fn header_one(c: &Catalog, name: &str) -> Result<Option<serde_json::Value>> {
            $(if name == stringify!($h) {
                return serde_json::to_value(&c.$h).map(Some).map_err(|e| internal(e.to_string()));
            })*
            Ok(None)
        }

        /// The header fields `m` has, set in `c`.
        fn set_header_fields(c: &mut Catalog, m: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
            $(if let Some(v) = m.get(stringify!($h)) {
                c.$h = serde_json::from_value(v.clone()).map_err(|e| internal(e.to_string()))?;
            })*
            Ok(())
        }

        /// One record (None: its entry removed) applied to `c`.
        fn apply_one(c: &mut Catalog, tag: u8, oid: u32, v: Option<&[u8]>) -> Result<()> {
            match tag {
                $($mtag => {
                    match v {
                        Some(v) => c.$m.load_one(oid, parse(v)?),
                        None => c.$m.remove_one(oid),
                    }
                    Ok(())
                })*
                $($stag => {
                    match v {
                        Some(_) => {
                            c.$s.insert(oid);
                        }
                        None => {
                            c.$s.remove(&oid);
                        }
                    }
                    Ok(())
                })*
                _ => Err(internal(format!("corrupt catalog: unknown record tag {tag}"))),
            }
        }
    };
}

// Tags are stored: never reuse or renumber one.
stored_fields! {
    maps: [
        schemas = 1 by_value,
        tables = 2 by_ptr,
        indexes = 3 by_ptr,
        views = 4 by_ptr,
        sequences = 5 by_ptr,
        enums = 6 by_ptr,
        composites = 7 by_ptr,
        composite_domains = 8 by_value,
        multirange_schemas = 9 by_value,
        array_names = 10 by_value,
        ranges = 11 by_ptr,
        domains = 12 by_ptr,
        functions = 13 by_ptr,
        roles = 14 by_value,
        extension_info = 15 by_value,
        triggers = 16 by_ptr,
        policies = 17 by_ptr,
        relstats = 18 by_value,
        statistics = 19 by_ptr,
        operators = 20 by_ptr,
        access_methods = 21 by_ptr,
        opfamilies = 22 by_ptr,
        opclasses = 23 by_ptr,
        opmembers = 24 by_ptr,
        casts = 25 by_ptr,
        rel_am = 26 by_value,
        base_types = 27 by_ptr,
        conversions = 28 by_ptr,
        languages = 29 by_ptr,
        fdws = 30 by_ptr,
        servers = 31 by_ptr,
        user_mappings = 32 by_ptr,
        transforms = 33 by_ptr,
        tablespaces = 34 by_value,
        rel_tablespace = 35 by_value,
        subscriptions = 36 by_value,
        publications = 37 by_ptr,
        colstats = 38 by_value,
        ts_configs = 39 by_ptr,
        ts_dicts = 40 by_ptr,
        ts_parsers = 41 by_ptr,
        ts_templates = 42 by_ptr,
        collations = 43 by_ptr,
        frozen_xids = 44 by_value,
        filenodes = 45 by_value,
        class_order = 46 by_value,
        rules = 47 by_ptr,
        event_triggers = 48 by_ptr,
        builtin_proc_owners = 49 by_value,
        comments = 50 by_ptr,
    ],
    sets: [
        opmembers_removed = 100,
        ts_removed = 101,
        had_triggers = 102,
        class_has_triggers = 103,
        class_has_subclass = 104,
        class_has_rules = 105,
    ],
    header: [
        index_key_format,
        storage_format,
        version,
        next_oid,
        comment_tick,
        class_comments,
        extensions,
        acls,
        default_acls,
        members,
        role_settings,
        depend_edits,
        parameters,
        shared_stamp,
        class_tick,
    ],
    // Not stored: rebuilt on load (`reindex`), the process's oid counter, the format read,
    // the database's locale (the cluster's registry keeps it; `Database::set_locale`), a
    // detach view's left-out partitions (`Catalog::detach_view`).
    derived: [oids, names, children, copies, catalog_format, lookups, db_locale, view],
}

thread_local! {
    static WRITTEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many bytes of catalog records (keys and values) commits on this thread have
/// written: a statement's mustn't grow with the catalog.
pub fn bytes_written() -> u64 {
    WRITTEN.with(|w| w.get())
}

/// The writes that store catalog `new` over `old` (the catalog stored now): the header, and
/// the records of the entries that differ.
pub fn writes(old: &Catalog, new: &Catalog) -> Result<Writes> {
    let mut out = vec![(crate::txn::CATALOG_KEY.to_vec(), Some(header(new)?))];
    if old.catalog_format < CATALOG_FORMAT {
        // (The stored catalog is one value: every record is new.)
        all_records(new, &mut out)?;
    } else {
        diff_records(old, new, &mut out)?;
    }
    let n: usize = out
        .iter()
        .map(|(k, v)| k.len() + v.as_ref().map_or(0, |v| v.len()))
        .sum();
    WRITTEN.with(|w| w.set(w.get() + n as u64));
    Ok(out)
}

/// `writes`, or none when `new` stores as `old` does.
pub fn changed(old: &Catalog, new: &Catalog) -> Result<Writes> {
    let mut out = Vec::new();
    diff_records(old, new, &mut out)?;
    let head = header(new)?;
    if out.is_empty() && head == header(old)? {
        return Ok(out);
    }
    out.insert(0, (crate::txn::CATALOG_KEY.to_vec(), Some(head)));
    Ok(out)
}

/// For a merge of concurrent DDL (`txn::catmerge`): the per-object records that differ
/// between `a` and `b`, as (tag, oid, `b`'s record or None where `b` has none), found by
/// comparing the maps as a commit does (cost follows what changed).
pub fn record_changes(a: &Catalog, b: &Catalog) -> Result<Vec<(u8, u32, Option<Bytes>)>> {
    let mut out = Vec::new();
    diff_records(a, b, &mut out)?;
    Ok(out
        .into_iter()
        .map(|(k, v)| {
            let r = &k[RECORD_PREFIX.len()..];
            (r[0], u32::from_be_bytes([r[1], r[2], r[3], r[4]]), v)
        })
        .collect())
}

/// One record applied to `c` (None: its entry removed).
pub fn apply_record(c: &mut Catalog, tag: u8, oid: u32, v: Option<&[u8]>) -> Result<()> {
    apply_one(c, tag, oid, v)
}

/// For a merge of concurrent DDL (`txn::catmerge`): the (tag, oid) of the records that
/// differ between `a` and `b`, found as a commit's diff finds them, none made.
pub fn changed_record_keys(a: &Catalog, b: &Catalog) -> Vec<(u8, u32)> {
    let mut out = Vec::new();
    changed_records(a, b, &mut out);
    out
}

/// Whether record (`tag`, `oid`) is alike in `a` and `b` (by pointer where entries are
/// shared).
pub fn same_record(a: &Catalog, b: &Catalog, tag: u8, oid: u32) -> bool {
    same_one(a, b, tag, oid)
}

/// Record (`tag`, `oid`) of `from` put in `to`, the entry itself (no JSON).
pub fn copy_record(from: &Catalog, to: &mut Catalog, tag: u8, oid: u32) {
    copy_one(from, to, tag, oid)
}

/// Record (`tag`, `oid`) of `c` as JSON (None: none).
pub fn record_json(c: &Catalog, tag: u8, oid: u32) -> Result<Option<serde_json::Value>> {
    one_json(c, tag, oid)
}

/// The header record's fields as JSON.
pub fn header_fields(c: &Catalog) -> Result<serde_json::Map<String, serde_json::Value>> {
    header_map(c)
}

/// The header fields in which `a` and `b` differ.
pub fn header_changed(a: &Catalog, b: &Catalog) -> Vec<&'static str> {
    header_diff(a, b)
}

/// One header field as JSON.
pub fn header_field(c: &Catalog, name: &str) -> Result<Option<serde_json::Value>> {
    header_one(c, name)
}

/// Header fields set from JSON (those `m` has).
pub fn set_header(c: &mut Catalog, m: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    set_header_fields(c, m)
}

/// The record tag of `class_order` (pg_class's heap order, merged apart).
pub const CLASS_ORDER_TAG: u8 = 46;

/// The record tag of `comments` (merged apart, `txn::catmerge`).
pub const COMMENTS_TAG: u8 = 50;

/// The writes that store `c` in a store holding no records yet.
pub fn writes_new(c: &Catalog) -> Result<Writes> {
    let mut out = vec![(crate::txn::CATALOG_KEY.to_vec(), Some(header(c)?))];
    all_records(c, &mut out)?;
    Ok(out)
}

/// The catalog whose header is `head`, its records read through `scan` (a key range's, in
/// key order). A header of format 0 is a whole catalog, and no record is read.
///
/// (A database's first open reads every record. Each map's records are scanned and parsed
/// on threads of their own, the tags spread over up to 8, and inserted in tag order: the
/// tables', indexes', pg_class order's ... scans and parses ran one after the other.)
pub fn load(
    head: &[u8],
    scan: impl Fn(&[u8], &[u8]) -> Result<Vec<(Vec<u8>, Bytes)>> + Sync,
) -> Result<Catalog> {
    let mut c: Catalog =
        serde_json::from_slice(head).map_err(|e| internal(format!("corrupt catalog: {e}")))?;
    if c.catalog_format > CATALOG_FORMAT {
        return Err(internal(format!(
            "the catalog is stored in format {}, which this build (format {CATALOG_FORMAT}) can't read",
            c.catalog_format
        )));
    }
    if c.catalog_format != 0 {
        let tags = all_tags();
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8);
        let cat = &c;
        let scan = &scan;
        let one = |tag: u8| -> Result<Option<Parsed>> {
            let mut from = RECORD_PREFIX.to_vec();
            from.push(tag);
            let mut to = RECORD_PREFIX.to_vec();
            to.push(tag + 1);
            let records = scan(&from, &to)?;
            if records.is_empty() {
                return Ok(None);
            }
            let mut items: Vec<(u32, &[u8])> = Vec::with_capacity(records.len());
            for (k, v) in &records {
                let rest = &k[RECORD_PREFIX.len()..];
                if rest.len() != 5 {
                    return Err(internal("corrupt catalog: bad record key"));
                }
                items.push((u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]), &v[..]));
            }
            parse_tag(cat, tag, &items).map(Some)
        };
        let parsed: Vec<(u8, Option<Parsed>)> = std::thread::scope(|s| -> Result<Vec<_>> {
            let parts: Vec<_> = (0..workers)
                .map(|w| {
                    let mine: Vec<u8> = tags.iter().copied().skip(w).step_by(workers).collect();
                    s.spawn(move || mine.into_iter().map(|t| Ok((t, one(t)?))).collect::<Result<Vec<_>>>())
                })
                .collect();
            let mut out = Vec::new();
            for p in parts {
                out.extend(p.join().map_err(|_| internal("catalog load panicked"))??);
            }
            Ok(out)
        })?;
        let mut parsed = parsed;
        parsed.sort_unstable_by_key(|p| p.0);
        for (tag, p) in parsed {
            if let Some(p) = p {
                insert_tag(&mut c, tag, p)?;
            }
        }
    }
    // (Comments read from a header of before format 2 are ticked from 1.)
    let top = (c.comments.values()).flat_map(|x| x.0.iter().map(|e| e.1)).max().unwrap_or(0);
    c.comment_tick = c.comment_tick.max(top);
    Ok(c)
}

/// Downgrading to a build of catalog format 1 (`roll-engine --downgrade-catalog`, after a
/// clean shutdown): the writes that store `c` as format 1 had it, comments in the header
/// (as a list in pg_description's order) and no comment records.
pub fn downgrade_writes(c: &Catalog) -> Result<Writes> {
    let mut m = header_map(c)?;
    m.insert("catalog_format".into(), 1.into());
    m.remove("comment_tick");
    let list: Vec<(u32, i32, &str)> = c.comments_in_order();
    m.insert("comments".into(), serde_json::to_value(&list).map_err(|e| internal(e.to_string()))?);
    let mut out = vec![(crate::txn::CATALOG_KEY.to_vec(), Some(json(&m)?))];
    for oid in c.comments.keys() {
        out.push((record_key(COMMENTS_TAG, *oid), None));
    }
    Ok(out)
}

/// Testing the upgrade: `c` stored as one value, as before catalog format 1 (with
/// `writes_new`'s records deleted).
#[doc(hidden)]
pub fn legacy_writes(c: &Catalog, records: Vec<Vec<u8>>) -> Result<Writes> {
    let mut c = c.clone();
    c.catalog_format = 0;
    let mut out = vec![(crate::txn::CATALOG_KEY.to_vec(), Some(json(&c)?))];
    out.extend(records.into_iter().map(|k| (k, None)));
    Ok(out)
}
