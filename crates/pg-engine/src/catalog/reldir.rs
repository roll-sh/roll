//! Catalog format 5: each relation's directory record (`RelDir`), what pg_class's rows and
//! directory (`system::class_keys`) read of a table, view, sequence or index entry. The
//! relation maps' indexes (`relindex`) keep one per entry, made from the entry as it is
//! stored (`RelDir::of_*`), and a commit stores them as records of their own in a compact
//! binary form (`encode`): a database's first read of pg_class or a statistics view reads
//! and decodes them (about 40 bytes a relation) instead of every entry's JSON (a table's is
//! 0.6 kB, parsed into 1.7 kB of structures).
//!
//! What a record holds is the entry's alone; what depends on other entries (an index's
//! schema, kind and persistence are its table's, a TOAST table's schema its table's
//! temporary schema's) is looked up when read.

use super::persist::{Writes, record_key};
use super::pmap::{PMap, Reader, Source};
use super::stored_ix::{LAZY_MIN, Part, tag_range};
use super::{Index, Sequence, Table, ViewDef};
use crate::error::{Result, internal};
use bytes::Bytes;
use std::sync::Arc;

/// `RelDir::flags`.
pub const TEMP: u16 = 1;
pub const UNLOGGED: u16 = 2;
/// relhasindex: a table's (a materialized view's storage's) indexes.
pub const HASINDEX: u16 = 4;
pub const RLS: u16 = 8;
pub const RLS_FORCE: u16 = 16;
/// relispartition: a partition, or an index attached to a partitioned index.
pub const PARTITION: u16 = 32;
/// A materialized view populated.
pub const POPULATED: u16 = 64;
/// A sequence's persistence given (`Sequence::unlogged` is Some; UNLOGGED its value).
pub const SEQ_PERSISTENCE: u16 = 128;

/// The record's encoding (its first byte).
const VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RelDir {
    pub name: Box<str>,
    /// relnamespace (an index's is its table's: 0 here).
    pub schema: u32,
    /// relowner (an index's is its table's: 0 here).
    pub owner: u32,
    /// relkind as the entry has it: `r`, `p`, `f` (tables), `v`, `m` (views), `S`, `i` (an
    /// index; `I` when its table is partitioned, which its table's record says).
    pub kind: u8,
    pub flags: u16,
    /// relnatts.
    pub natts: u16,
    /// relchecks.
    pub checks: u16,
    /// reltype.
    pub rowtype: u32,
    /// reloftype.
    pub of_type: u32,
    /// reltoastrelid (its index has the next oid).
    pub toast: u32,
    /// An index's table (a materialized view's for its storage's); a sequence's owning table
    /// (0: none).
    pub link: u32,
    /// relam unless `Catalog::rel_am` says otherwise.
    pub am: u32,
    /// relreplident of a table (0: DEFAULT).
    pub replident: u8,
    /// A table's (a materialized view's storage's) indexes, its primary key's among them, in
    /// oid order: what pg_index has of it.
    pub indexes: Box<[u32]>,
    pub extra: Option<Box<DirExtra>>,
}

/// What few relations have.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DirExtra {
    /// WITH (storage parameters) as `name=value` (a table's `toast.` ones its TOAST table's).
    pub options: Vec<String>,
    /// A view's WITH CHECK OPTION.
    pub check_option: Option<String>,
    /// A TOAST table's and TOAST index's names, when renamed.
    pub toast_relname: Option<String>,
    pub toast_indname: Option<String>,
}

fn extra(e: DirExtra) -> Option<Box<DirExtra>> {
    (e != DirExtra::default()).then(|| Box::new(e))
}

fn own_indexes(t: &Table) -> Box<[u32]> {
    let mut v: Vec<u32> = t.indexes.iter().chain(t.pkey_index.iter()).copied().collect();
    v.sort_unstable();
    v.dedup();
    v.into()
}

fn storage_flags(t: &Table) -> u16 {
    let mut f = 0;
    if t.temporary {
        f |= TEMP;
    }
    if t.unlogged {
        f |= UNLOGGED;
    }
    if !t.indexes.is_empty() || t.pkey_index.is_some() {
        f |= HASINDEX;
    }
    f
}

impl RelDir {
    pub fn of_table(t: &Table) -> RelDir {
        let mut flags = storage_flags(t);
        for (on, f) in [(t.rls, RLS), (t.rls_force, RLS_FORCE), (t.partbound.is_some(), PARTITION)] {
            if on {
                flags |= f;
            }
        }
        RelDir {
            name: t.name.as_str().into(),
            schema: t.schema,
            owner: t.owner,
            kind: t.relkind() as u8,
            flags,
            natts: t.live_columns().count() as u16,
            checks: t.checks().count() as u16,
            rowtype: t.rowtype,
            of_type: t.of_type,
            toast: t.toast,
            link: 0,
            am: if t.partkey.is_some() || t.foreign.is_some() { 0 } else { 2 },
            replident: t.replident.map_or(0, |c| c as u8),
            indexes: own_indexes(t),
            extra: extra(DirExtra {
                options: t.options.clone(),
                check_option: None,
                toast_relname: t.toast_relname.clone(),
                toast_indname: t.toast_indname.clone(),
            }),
        }
    }

    pub fn of_view(v: &ViewDef) -> RelDir {
        let st = v.storage.as_deref();
        let mut flags = st.map_or(0, storage_flags);
        if v.populated {
            flags |= POPULATED;
        }
        // (A materialized view's reloptions are its storage's; a view's its own.)
        let options = match (v.materialized, st) {
            (true, Some(t)) => t.options.clone(),
            (true, None) => Vec::new(),
            (false, _) => v.options.clone(),
        };
        RelDir {
            name: v.name.as_str().into(),
            schema: v.schema,
            owner: v.owner,
            kind: if v.materialized { b'm' } else { b'v' },
            flags,
            natts: v.columns.len() as u16,
            checks: 0,
            rowtype: v.rowtype,
            of_type: 0,
            toast: st.map_or(0, |t| t.toast),
            link: 0,
            am: if v.materialized { 2 } else { 0 },
            replident: 0,
            indexes: st.map_or_else(|| Box::new([]) as Box<[u32]>, own_indexes),
            extra: extra(DirExtra {
                options,
                check_option: v.check_option.clone().filter(|_| !v.materialized),
                toast_relname: st.and_then(|t| t.toast_relname.clone()),
                toast_indname: st.and_then(|t| t.toast_indname.clone()),
            }),
        }
    }

    pub fn of_sequence(s: &Sequence) -> RelDir {
        let flags = match s.unlogged {
            None => 0,
            Some(true) => SEQ_PERSISTENCE | UNLOGGED,
            Some(false) => SEQ_PERSISTENCE,
        };
        RelDir {
            name: s.name.as_str().into(),
            schema: s.schema,
            owner: s.owner,
            kind: b'S',
            flags,
            natts: 3,
            link: s.owned_by.map_or(0, |o| o.0),
            ..RelDir::default()
        }
    }

    pub fn of_index(i: &Index) -> RelDir {
        RelDir {
            name: i.name.as_str().into(),
            kind: b'i',
            flags: if i.parent != 0 { PARTITION } else { 0 },
            natts: (i.columns.len() + i.include.len()) as u16,
            link: i.table,
            am: super::system::access_method_oid(&i.method),
            extra: extra(DirExtra { options: i.options.clone(), ..DirExtra::default() }),
            ..RelDir::default()
        }
    }

    pub fn has(&self, f: u16) -> bool {
        self.flags & f != 0
    }

    pub fn options(&self) -> &[String] {
        self.extra.as_ref().map_or(&[], |e| &e.options)
    }

    /// relpersistence of a table, a materialized view's storage, or an index of one.
    pub fn persistence(&self) -> char {
        if self.has(TEMP) {
            't'
        } else if self.has(UNLOGGED) {
            'u'
        } else {
            'p'
        }
    }

    /// Its TOAST table's (false) or TOAST index's (true) name.
    pub fn toast_relname(&self, oid: u32, index: bool) -> String {
        let e = self.extra.as_deref();
        let given = e.and_then(|e| if index { e.toast_indname.as_ref() } else { e.toast_relname.as_ref() });
        given.cloned().unwrap_or_else(|| super::toast_name(oid, index))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(16 + self.name.len() + 2 * self.indexes.len());
        o.push(VERSION);
        o.push(self.kind);
        let has_extra = self.extra.is_some() as u64;
        for x in [self.flags as u64, has_extra, self.name.len() as u64] {
            put(&mut o, x);
        }
        o.extend_from_slice(self.name.as_bytes());
        for x in [self.schema, self.owner, self.natts as u32, self.checks as u32, self.rowtype, self.of_type] {
            put(&mut o, x as u64);
        }
        for x in [self.toast, self.link, self.am, self.replident as u32, self.indexes.len() as u32] {
            put(&mut o, x as u64);
        }
        for i in self.indexes.iter() {
            put(&mut o, *i as u64);
        }
        if let Some(e) = &self.extra {
            put(&mut o, e.options.len() as u64);
            for s in &e.options {
                put_str(&mut o, s);
            }
            for s in [&e.check_option, &e.toast_relname, &e.toast_indname] {
                match s {
                    None => o.push(0),
                    Some(s) => {
                        o.push(1);
                        put_str(&mut o, s);
                    }
                }
            }
        }
        o
    }

    pub fn decode(b: &[u8]) -> Result<RelDir> {
        let mut r = Rd { b, i: 0 };
        if r.byte()? != VERSION {
            return Err(internal("corrupt catalog: unknown relation directory record"));
        }
        let kind = r.byte()?;
        let flags = r.num()? as u16;
        let has_extra = r.num()? != 0;
        let name = r.str()?.into();
        let mut d = RelDir {
            name,
            kind,
            flags,
            schema: r.num()?,
            owner: r.num()?,
            natts: r.num()? as u16,
            checks: r.num()? as u16,
            rowtype: r.num()?,
            of_type: r.num()?,
            toast: r.num()?,
            link: r.num()?,
            am: r.num()?,
            replident: r.num()? as u8,
            ..RelDir::default()
        };
        let n = r.num()? as usize;
        d.indexes = (0..n).map(|_| r.num()).collect::<Result<Vec<u32>>>()?.into();
        if has_extra {
            let n = r.num()? as usize;
            let mut e = DirExtra { options: (0..n).map(|_| r.str()).collect::<Result<_>>()?, ..DirExtra::default() };
            for s in [&mut e.check_option, &mut e.toast_relname, &mut e.toast_indname] {
                if r.byte()? != 0 {
                    *s = Some(r.str()?);
                }
            }
            d.extra = Some(Box::new(e));
        }
        if r.i != b.len() {
            return Err(internal("corrupt catalog: relation directory record too long"));
        }
        Ok(d)
    }
}

/// A relation map's directory records, by relation oid (a `relindex` index's part, stored
/// under its tag in `encode`'s form).
#[derive(Clone, Default)]
pub struct DirMap(pub PMap<Arc<RelDir>>);

impl std::ops::Deref for DirMap {
    type Target = PMap<Arc<RelDir>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DirMap {
    /// `d` as relation `oid`'s record (unchanged when alike: its chunk isn't copied).
    pub fn set(&mut self, oid: u32, d: RelDir) {
        if self.0.get(&oid).is_none_or(|x| **x != d) {
            self.0.insert(oid, Arc::new(d));
        }
    }

    pub fn unset(&mut self, oid: u32) {
        self.0.remove(&oid);
    }
}

fn decode_arc(b: &[u8]) -> Result<Arc<RelDir>> {
    RelDir::decode(b).map(Arc::new)
}

fn bytes(d: &RelDir) -> Bytes {
    Bytes::from(d.encode())
}

impl Part for DirMap {
    fn shape(&self) -> (usize, u32) {
        (self.0.len(), self.0.last_key().unwrap_or(0))
    }
    fn diff_part(&self, tag: u8, new: &Self, out: &mut Writes) -> Result<()> {
        self.0.diff(&new.0, |a, b| a == b, |k, _, b| out.push((record_key(tag, k), b.map(|d| bytes(d)))));
        Ok(())
    }
    fn put_part(&self, tag: u8, del: bool, out: &mut Writes) -> Result<()> {
        for (k, d) in &self.0 {
            out.push((record_key(tag, *k), (!del).then(|| bytes(d))));
        }
        Ok(())
    }
    fn open_part(tag: u8, shape: Option<(usize, u32)>, read: &Reader) -> Result<Self> {
        let (len, last) = shape.unwrap_or((0, 0));
        let (from, to) = tag_range(tag);
        if len >= LAZY_MIN {
            let src = Source { prefix: from, end: to, read: read.clone(), parse: decode_arc };
            return Ok(DirMap(PMap::stored(src, len, last)));
        }
        let mut m = PMap::new();
        for (k, v) in read(&from, &to)? {
            let k = <[u8; 4]>::try_from(&k[from.len()..]).map_err(|_| internal("corrupt catalog: bad record key"))?;
            m.insert(u32::from_be_bytes(k), decode_arc(&v)?);
        }
        Ok(DirMap(m))
    }
    fn read_all(&self) {
        self.0.read_all()
    }
}

impl super::Catalog {
    /// The directory record of table `oid`, or of materialized view `oid`'s storage (what
    /// `index_table` finds).
    pub fn storage_dir(&self, oid: u32) -> Option<&RelDir> {
        match self.tables.ix().dir.get(&oid) {
            Some(d) => Some(d),
            None => self.views.ix().dir.get(&oid).map(|d| &**d).filter(|d| d.kind == b'm'),
        }
    }

    /// The relation whose TOAST table (false) or TOAST index (true) `oid` is, with its
    /// directory record (`toast_owner` without its entry).
    pub fn toast_dir(&self, oid: u32) -> Option<(u32, bool, &RelDir)> {
        let (rel, d) = match self.tables.ix().toast.get(&oid) {
            Some(&rel) => (rel, self.tables.ix().dir.get(&rel)?),
            None => {
                let rel = *self.views.ix().toast.get(&oid)?;
                (rel, self.views.ix().dir.get(&rel)?)
            }
        };
        Some((rel, d.toast != oid, d))
    }

    /// The relations with a TOAST table, by oid: tables, then materialized views.
    pub fn toasted_dirs(&self) -> impl Iterator<Item = (u32, &RelDir)> {
        let t = self.tables.ix().dir.iter();
        (t.chain(self.views.ix().dir.iter())).map(|(k, d)| (*k, &**d)).filter(|(_, d)| d.toast != 0)
    }

    /// A TOAST table's schema: pg_toast, or a temporary table's `pg_toast_temp_N`.
    pub fn toast_schema_of(&self, d: &RelDir) -> u32 {
        if !d.has(TEMP) {
            return super::PG_TOAST_OID;
        }
        let name = self.schema_name(d.schema).replacen("pg_temp_", "pg_toast_temp_", 1);
        self.schema_by_name(&name).map_or(super::PG_TOAST_OID, |s| s.oid)
    }
}

/// Debug builds: the directory records (of `only` alone when given) are those the entries
/// make, and every entry has one.
#[cfg(debug_assertions)]
pub fn check(c: &super::Catalog, only: Option<u32>) {
    fn one<V>(m: &PMap<V>, dirs: &DirMap, only: Option<u32>, of: fn(&V) -> RelDir, what: &str) {
        let keys: Vec<u32> = match only {
            Some(o) => vec![o],
            None => m.keys().chain(dirs.keys()).copied().collect(),
        };
        for k in keys {
            let want = m.get(&k).map(of);
            assert_eq!(dirs.get(&k).map(|d| (**d).clone()), want, "the directory record of {what} {k}");
        }
    }
    one(&c.tables, &c.tables.ix().dir, only, |t| RelDir::of_table(t), "table");
    one(&c.views, &c.views.ix().dir, only, |v| RelDir::of_view(v), "view");
    one(&c.sequences, &c.sequences.ix().dir, only, |s| RelDir::of_sequence(s), "sequence");
    one(&c.indexes, &c.indexes.ix().dir, only, |i| RelDir::of_index(i), "index");
}

/// The directory records of every relation entry, made from the entries (a catalog of
/// format 4, which had none: its upgrade to format 5 reads every relation once).
pub fn fill(c: &mut super::Catalog) {
    let tables = DirMap(c.tables.iter().map(|(k, t)| (*k, Arc::new(RelDir::of_table(t)))).collect());
    c.tables.parts_mut().1.dir = tables;
    let views = DirMap(c.views.iter().map(|(k, v)| (*k, Arc::new(RelDir::of_view(v)))).collect());
    c.views.parts_mut().1.dir = views;
    let seqs = DirMap(c.sequences.iter().map(|(k, s)| (*k, Arc::new(RelDir::of_sequence(s)))).collect());
    c.sequences.parts_mut().1.dir = seqs;
    let ixs = DirMap(c.indexes.iter().map(|(k, i)| (*k, Arc::new(RelDir::of_index(i)))).collect());
    c.indexes.parts_mut().1.dir = ixs;
}

fn put(o: &mut Vec<u8>, mut x: u64) {
    while x >= 0x80 {
        o.push(x as u8 | 0x80);
        x >>= 7;
    }
    o.push(x as u8);
}

fn put_str(o: &mut Vec<u8>, s: &str) {
    put(o, s.len() as u64);
    o.extend_from_slice(s.as_bytes());
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

fn short() -> crate::error::Error {
    internal("corrupt catalog: relation directory record cut short")
}

impl Rd<'_> {
    fn byte(&mut self) -> Result<u8> {
        let x = *self.b.get(self.i).ok_or_else(short)?;
        self.i += 1;
        Ok(x)
    }

    fn num(&mut self) -> Result<u32> {
        let mut x: u64 = 0;
        for shift in (0..35).step_by(7) {
            let c = self.byte()?;
            x |= ((c & 0x7f) as u64) << shift;
            if c < 0x80 {
                return u32::try_from(x).map_err(|_| short());
            }
        }
        Err(short())
    }

    fn str(&mut self) -> Result<String> {
        let n = self.num()? as usize;
        let s = self.b.get(self.i..self.i + n).ok_or_else(short)?;
        self.i += n;
        String::from_utf8(s.to_vec()).map_err(|_| internal("corrupt catalog: relation directory record's name"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let mut d = RelDir {
            name: "t5005".into(),
            schema: 2200,
            owner: 10,
            kind: b'r',
            flags: HASINDEX | RLS,
            natts: 3,
            rowtype: 16_390,
            toast: 300_000,
            am: 2,
            indexes: vec![16_395, 1 << 31].into(),
            ..RelDir::default()
        };
        assert_eq!(RelDir::decode(&d.encode()).unwrap(), d);
        d.extra = Some(Box::new(DirExtra {
            options: vec!["fillfactor=70".into(), "toast.autovacuum_enabled=off".into()],
            check_option: None,
            toast_relname: Some("renamed".into()),
            toast_indname: None,
        }));
        let b = d.encode();
        assert_eq!(RelDir::decode(&b).unwrap(), d);
        assert!(RelDir::decode(&b[..b.len() - 1]).is_err());
    }
}
