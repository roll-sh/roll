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
pub const CATALOG_FORMAT: u32 = 1;

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

        /// One map's (or set's) records, in oid order: the values parsed on several threads
        /// (`parse_all`), inserted in order.
        fn load_records(c: &mut Catalog, tag: u8, items: &[(u32, &[u8])]) -> Result<()> {
            match tag {
                $($mtag => {
                    for (oid, v) in parse_all(items)? {
                        c.$m.load_one(oid, v);
                    }
                    Ok(())
                })*
                $($stag => {
                    c.$s.extend(items.iter().map(|(oid, _)| *oid));
                    Ok(())
                })*
                _ => Err(internal(format!("corrupt catalog: unknown record tag {tag}"))),
            }
        }

        fn header(c: &Catalog) -> Result<Bytes> {
            let mut m = serde_json::Map::new();
            m.insert("catalog_format".into(), CATALOG_FORMAT.into());
            $(m.insert(
                stringify!($h).into(),
                serde_json::to_value(&c.$h).map_err(|e| internal(e.to_string()))?,
            );)*
            json(&m)
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
        comments,
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
    // the database's locale (the cluster's registry keeps it; `Database::set_locale`).
    derived: [oids, names, children, copies, catalog_format, lookups, db_locale],
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

/// The writes that store `c` in a store holding no records yet.
pub fn writes_new(c: &Catalog) -> Result<Writes> {
    let mut out = vec![(crate::txn::CATALOG_KEY.to_vec(), Some(header(c)?))];
    all_records(c, &mut out)?;
    Ok(out)
}

/// The catalog whose header is `head`, its records read from `records` (in key order).
/// A header of format 0 is a whole catalog, and `records` isn't read.
pub fn load(
    head: &[u8],
    records: impl FnOnce() -> Result<Vec<(Vec<u8>, Bytes)>>,
) -> Result<Catalog> {
    let mut c: Catalog =
        serde_json::from_slice(head).map_err(|e| internal(format!("corrupt catalog: {e}")))?;
    if c.catalog_format == 0 {
        return Ok(c);
    }
    let records = records()?;
    // (In key order: each map's records together, in oid order.)
    let mut items: Vec<(u32, &[u8])> = Vec::new();
    let mut tag = None;
    for (k, v) in &records {
        let rest = &k[RECORD_PREFIX.len()..];
        if rest.len() != 5 {
            return Err(internal("corrupt catalog: bad record key"));
        }
        if tag.is_some_and(|t| t != rest[0]) {
            load_records(&mut c, tag.unwrap(), &items)?;
            items.clear();
        }
        tag = Some(rest[0]);
        items.push((u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]), &v[..]));
    }
    if let Some(t) = tag {
        load_records(&mut c, t, &items)?;
    }
    Ok(c)
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
