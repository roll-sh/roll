//! Catalog format 4: a database opens without reading its objects. The header carries each
//! large map's shape (its entry count and last key) and the small parts of its indexes; the
//! maps of relations, functions and their per-relation data (`LAZY_TAGS`) and the indexes
//! over them (`relindex`'s, the relation name index, the inheritance children) are records
//! of their own, read a group (`pmap::PMap`) or a bucket (`pmap::PHash`) at a time as they
//! are first used, and parsed a chunk at a time. Before it, every open parsed every record
//! and built every index again: 90 ms at 10,000 tables, 0.6 s and 570 MB at 100,000.
//!
//! A commit writes an index's records as it writes the maps' (`diff_ix`): the entries that
//! differ, found by comparing groups and buckets by pointer.

use super::Catalog;
use super::persist::{RECORD_PREFIX, Writes, json, parse, record_key};
use super::pmap::{HSource, Key, PHash, PMap, Reader, Source};
use super::relindex::{FuncIx, IndexIx, SeqIx, TableIx, ViewIx};
use crate::error::{Result, internal};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;

/// A map with fewer entries is read whole when the catalog opens (one scan).
pub const LAZY_MIN: usize = 256;

/// The maps read on demand (their tags in `persist`'s `stored_fields!`).
pub const LAZY_TAGS: [u8; 12] = [2, 3, 4, 5, 13, 18, 26, 35, 44, 45, 46, 50];

/// The tags of the index records (never reused or renumbered; at and above `FIRST_IX_TAG`).
pub const FIRST_IX_TAG: u8 = 128;
const NAMES: u8 = 128;
const CHILDREN: u8 = 129;

/// Each stored map's (entry count, last key), by tag: the header's `stored`.
pub type Shapes = BTreeMap<u8, (usize, u32)>;

pub fn tag_range(tag: u8) -> (Vec<u8>, Vec<u8>) {
    let mut from = RECORD_PREFIX.to_vec();
    from.push(tag);
    let mut to = RECORD_PREFIX.to_vec();
    // (Tag 255 isn't used: its range would end past the records'.)
    to.push(tag + 1);
    (from, to)
}

/// A map stored a record per entry.
pub trait Part: Sized {
    fn shape(&self) -> (usize, u32);
    /// The writes turning `self`'s records into `new`'s.
    fn diff_part(&self, tag: u8, new: &Self, out: &mut Writes) -> Result<()>;
    /// Every record (`del`: its deletion instead).
    fn put_part(&self, tag: u8, del: bool, out: &mut Writes) -> Result<()>;
    /// The map stored under `tag`: read now when small, else as it is used.
    fn open_part(tag: u8, shape: Option<(usize, u32)>, read: &Reader) -> Result<Self>;
    /// Every group or bucket read now (`PMap::read_all`).
    fn read_all(&self);
}

fn parse_json<V: DeserializeOwned>(b: &[u8]) -> Result<V> {
    parse(b)
}

impl<V: Serialize + DeserializeOwned + Clone + PartialEq + Send + Sync + 'static> Part for PMap<V> {
    fn shape(&self) -> (usize, u32) {
        (self.len(), self.last_key().unwrap_or(0))
    }
    fn diff_part(&self, tag: u8, new: &Self, out: &mut Writes) -> Result<()> {
        let mut changed = Vec::new();
        self.diff(new, |a, b| a == b, |k, _, b| changed.push((k, b)));
        for (k, b) in changed {
            out.push((record_key(tag, k), b.map(json).transpose()?));
        }
        Ok(())
    }
    fn put_part(&self, tag: u8, del: bool, out: &mut Writes) -> Result<()> {
        for (k, v) in self {
            out.push((record_key(tag, *k), if del { None } else { Some(json(v)?) }));
        }
        Ok(())
    }
    fn open_part(tag: u8, shape: Option<(usize, u32)>, read: &Reader) -> Result<Self> {
        open_map(tag, shape, read)
    }
    fn read_all(&self) {
        PMap::read_all(self)
    }
}

/// The map stored under `tag` (a catalog map's entries or an index's): read now when small,
/// else group by group as it is used.
pub fn open_map<V: DeserializeOwned + Clone + Send + Sync + 'static>(
    tag: u8,
    shape: Option<(usize, u32)>,
    read: &Reader,
) -> Result<PMap<V>> {
    let (len, last) = shape.unwrap_or((0, 0));
    let (from, to) = tag_range(tag);
    if len >= LAZY_MIN {
        let src = Source { prefix: from, end: to, read: read.clone(), parse: parse_json::<V> };
        return Ok(PMap::stored(src, len, last));
    }
    let mut m = BTreeMap::new();
    for (k, v) in read(&from, &to)? {
        let k = <[u8; 4]>::try_from(&k[from.len()..]).map_err(|_| internal("corrupt catalog: bad record key"))?;
        m.insert(u32::from_be_bytes(k), parse(&v)?);
    }
    Ok(PMap::from(m))
}

fn map_shape<V>(m: &PMap<V>) -> (usize, u32) {
    (m.len(), m.last_key().unwrap_or(0))
}

/// An index stored as records: its maps (`Part`s, each under a tag of its own) and its
/// small parts (in the header's `ix`, under the map's name).
pub trait StoredIx: Default {
    fn diff_ix(&self, new: &Self, out: &mut Writes) -> Result<()>;
    fn put_ix(&self, del: bool, out: &mut Writes) -> Result<()>;
    fn shapes(&self, s: &mut Shapes);
    fn read_ix(&self);
    fn open_ix(s: &Shapes, read: &Reader) -> Result<Self>;
    fn head(&self) -> Result<serde_json::Value>;
    fn set_head(&mut self, v: &serde_json::Value) -> Result<()>;
}

fn to_json<T: Serialize>(v: &T) -> Result<serde_json::Value> {
    serde_json::to_value(v).map_err(|e| internal(e.to_string()))
}

fn from_json<T: DeserializeOwned>(v: &serde_json::Value) -> Result<T> {
    serde_json::from_value(v.clone()).map_err(|e| internal(format!("corrupt catalog: {e}")))
}

// Tags are stored: never reuse or renumber one.
macro_rules! stored_ix {
    ($ty:ident { parts: [$($f:ident = $t:literal),* $(,)?], head: [$($h:ident),* $(,)?] $(,)? }) => {
        // (Every field is a part or in the header: the destructuring is exhaustive, so an
        // index field added without a place here doesn't compile; it would be empty after
        // an open.)
        #[allow(dead_code)]
        const _: fn(&$ty) = |x| {
            let $ty { $($f: _,)* $($h: _,)* } = x;
        };
        impl StoredIx for $ty {
            fn diff_ix(&self, new: &Self, out: &mut Writes) -> Result<()> {
                $(self.$f.diff_part($t, &new.$f, out)?;)*
                Ok(())
            }
            fn put_ix(&self, del: bool, out: &mut Writes) -> Result<()> {
                $(self.$f.put_part($t, del, out)?;)*
                Ok(())
            }
            fn shapes(&self, s: &mut Shapes) {
                $(s.insert($t, Part::shape(&self.$f));)*
            }
            fn read_ix(&self) {
                $(Part::read_all(&self.$f);)*
            }
            fn open_ix(s: &Shapes, read: &Reader) -> Result<Self> {
                let mut x = Self::default();
                $(x.$f = Part::open_part($t, s.get(&$t).copied(), read)?;)*
                Ok(x)
            }
            fn head(&self) -> Result<serde_json::Value> {
                #[allow(unused_mut)]
                let mut m = serde_json::Map::new();
                $(m.insert(stringify!($h).into(), to_json(&self.$h)?);)*
                Ok(m.into())
            }
            fn set_head(&mut self, _v: &serde_json::Value) -> Result<()> {
                $(if let Some(x) = _v.get(stringify!($h)) {
                    self.$h = from_json(x)?;
                })*
                Ok(())
            }
        }
    };
}

stored_ix!(TableIx {
    parts: [
        con_names = 130,
        toast = 131,
        rowtypes = 132,
        row_users = 133,
        rls = 134,
        con_owner = 135,
        def_owner = 136,
        fk_refs = 137,
        ri_checks = 138,
        words = 139,
        detach_pending = 140,
        dir = 152,
    ],
    head: [trigger_marks, rows],
});

stored_ix!(ViewIx {
    parts: [toast = 141, rowtypes = 142, barriers = 143, words = 144, row_users = 145, rowtype_rels = 146, rules = 147, dir = 153],
    head: [rows],
});

stored_ix!(IndexIx { parts: [attached = 148, words = 149, dir = 154], head: [expr_tables, parents, rows] });

stored_ix!(SeqIx { parts: [owned = 150, dir = 155], head: [rows] });

stored_ix!(FuncIx { parts: [named = 151], head: [] });

/// A map of `LAZY_TAGS`: opened from its shape (an indexed map's indexes from theirs).
trait LazyMap {
    fn shape(&self) -> (usize, u32);
    fn open(&mut self, tag: u8, name: &str, s: &Shapes, ix: &serde_json::Value, read: &Reader) -> Result<()>;
    fn ix_shapes(&self, _s: &mut Shapes) {}
    fn ix_head(&self, _name: &str, _m: &mut serde_json::Map<String, serde_json::Value>) -> Result<()> {
        Ok(())
    }
    fn diff_ix(&self, _new: &Self, _out: &mut Writes) -> Result<()> {
        Ok(())
    }
    fn put_ix(&self, _del: bool, _out: &mut Writes) -> Result<()> {
        Ok(())
    }
    /// The map (and its indexes) read whole now.
    fn read(&self);
}

impl<V: DeserializeOwned + Clone + Send + Sync + 'static> LazyMap for PMap<V> {
    fn shape(&self) -> (usize, u32) {
        map_shape(self)
    }
    fn read(&self) {
        self.read_all()
    }
    fn open(&mut self, tag: u8, _: &str, s: &Shapes, _: &serde_json::Value, read: &Reader) -> Result<()> {
        *self = open_map(tag, s.get(&tag).copied(), read)?;
        Ok(())
    }
}

impl<V, I> LazyMap for super::pmap::IndexedMap<V, I>
where
    V: DeserializeOwned + Clone + Send + Sync + 'static,
    I: StoredIx + super::pmap::MapIndex<V>,
{
    fn shape(&self) -> (usize, u32) {
        map_shape::<V>(self)
    }
    fn open(&mut self, tag: u8, name: &str, s: &Shapes, ix: &serde_json::Value, read: &Reader) -> Result<()> {
        let (map, x) = self.parts_mut();
        *map = open_map(tag, s.get(&tag).copied(), read)?;
        *x = I::open_ix(s, read)?;
        if let Some(h) = ix.get(name) {
            x.set_head(h)?;
        }
        Ok(())
    }
    fn ix_shapes(&self, s: &mut Shapes) {
        self.ix().shapes(s)
    }
    fn ix_head(&self, name: &str, m: &mut serde_json::Map<String, serde_json::Value>) -> Result<()> {
        m.insert(name.into(), self.ix().head()?);
        Ok(())
    }
    fn diff_ix(&self, new: &Self, out: &mut Writes) -> Result<()> {
        self.ix().diff_ix(new.ix(), out)
    }
    fn put_ix(&self, del: bool, out: &mut Writes) -> Result<()> {
        self.ix().put_ix(del, out)
    }
    fn read(&self) {
        self.read_all();
        self.ix().read_ix();
    }
}

macro_rules! lazy_maps {
    ($($m:ident = $t:literal),* $(,)?) => {
        /// The header's `stored` (every stored map's shape) and `ix` (the indexes' small
        /// parts).
        pub fn header_extras(c: &Catalog) -> Result<(Shapes, serde_json::Value)> {
            let mut s = Shapes::new();
            let mut m = serde_json::Map::new();
            $(
                s.insert($t, LazyMap::shape(&c.$m));
                c.$m.ix_shapes(&mut s);
                c.$m.ix_head(stringify!($m), &mut m)?;
            )*
            s.insert(NAMES, Part::shape(&c.names));
            s.insert(CHILDREN, Part::shape(&c.children));
            Ok((s, m.into()))
        }

        /// The maps of `LAZY_TAGS` and the indexes, opened from the header's extras.
        pub fn open(c: &mut Catalog, s: &Shapes, ix: &serde_json::Value, read: &Reader) -> Result<()> {
            $(c.$m.open($t, stringify!($m), s, ix, read)?;)*
            c.names = Part::open_part(NAMES, s.get(&NAMES).copied(), read)?;
            c.children = Part::open_part(CHILDREN, s.get(&CHILDREN).copied(), read)?;
            Ok(())
        }

        /// The writes turning `a`'s index records into `b`'s.
        pub fn diff_ix(a: &Catalog, b: &Catalog, out: &mut Writes) -> Result<()> {
            $(a.$m.diff_ix(&b.$m, out)?;)*
            a.names.diff_part(NAMES, &b.names, out)?;
            a.children.diff_part(CHILDREN, &b.children, out)
        }

        /// Every index record of `c` (`del`: their deletions).
        pub fn put_ix(c: &Catalog, del: bool, out: &mut Writes) -> Result<()> {
            $(c.$m.put_ix(del, out)?;)*
            c.names.put_part(NAMES, del, out)?;
            c.children.put_part(CHILDREN, del, out)
        }

        /// Every stored map and index read whole now (`PMap::read_all`), before a whole
        /// system catalog's generation visits them.
        pub fn read_all(c: &Catalog) {
            $(c.$m.read();)*
            Part::read_all(&c.names);
            Part::read_all(&c.children);
        }

        #[cfg(test)]
        const LISTED: &[u8] = &[$($t),*];
    };
}

lazy_maps! {
    tables = 2,
    indexes = 3,
    views = 4,
    sequences = 5,
    functions = 13,
    relstats = 18,
    rel_am = 26,
    rel_tablespace = 35,
    frozen_xids = 44,
    filenodes = 45,
    class_order = 46,
    comments = 50,
}

#[cfg(test)]
#[test]
fn lazy_tags_are_the_listed_maps() {
    assert_eq!(LISTED, &LAZY_TAGS[..]);
}

/// A `PHash` entry's record key: the tag, the key's bucket, its bytes.
fn hash_key<K: Key>(tag: u8, k: &K) -> Vec<u8> {
    let mut out = record_key(tag, 0);
    out.truncate(RECORD_PREFIX.len() + 1);
    out.push(k.bucket() as u8);
    k.encode(&mut out);
    out
}

fn parse_entry<K: Key, V: DeserializeOwned>(k: &[u8], v: &[u8]) -> Result<(K, V)> {
    Ok((K::decode(k).ok_or_else(|| internal("corrupt catalog: bad index key"))?, parse(v)?))
}

impl<K: Key + Send + Sync + 'static, V: Serialize + DeserializeOwned + Clone + PartialEq + Send + Sync + 'static> Part
    for PHash<K, V>
{
    fn shape(&self) -> (usize, u32) {
        (self.len(), 0)
    }
    fn diff_part(&self, tag: u8, new: &Self, out: &mut Writes) -> Result<()> {
        let mut changed = Vec::new();
        self.diff(new, |a, b| a == b, |k, _, b| changed.push((k, b)));
        for (k, b) in changed {
            out.push((hash_key(tag, k), b.map(json).transpose()?));
        }
        Ok(())
    }
    fn put_part(&self, tag: u8, del: bool, out: &mut Writes) -> Result<()> {
        for (k, v) in self.iter() {
            out.push((hash_key(tag, k), if del { None } else { Some(json(v)?) }));
        }
        Ok(())
    }
    fn open_part(tag: u8, shape: Option<(usize, u32)>, read: &Reader) -> Result<Self> {
        let len = shape.map_or(0, |s| s.0);
        let (from, to) = tag_range(tag);
        if len >= LAZY_MIN {
            let src = HSource { prefix: from, end: to, read: read.clone(), parse: parse_entry::<K, V> };
            return Ok(PHash::stored(src, len));
        }
        let mut m = PHash::default();
        for (k, v) in read(&from, &to)? {
            let (k, v) = parse_entry::<K, V>(&k[from.len() + 1..], &v)?;
            m.insert(k, v);
        }
        Ok(m)
    }
    fn read_all(&self) {
        PHash::read_all(self)
    }
}
