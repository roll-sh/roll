//! Large objects: the pg_largeobject_metadata and pg_largeobject catalogs (Postgres's
//! pg_largeobject.c and inv_api.c) and the descriptors the lo_* functions use (be-fsstubs.c).
//!
//! Metadata and data pages live in reserved keyspaces of the database's store, not in the
//! catalog blob: they are transactional row data, so sessions create and write large objects
//! concurrently without schema-change conflicts.

use crate::catalog::Catalog;
use crate::catalog::acl::{self, Acl, ObjKind};
use crate::error::{Error, Result, code};
use crate::storage::keys::prefix_end;
use crate::txn::Txn;
use crate::txn::locks::{LockMode, WaitPolicy};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

type Writes = BTreeMap<Vec<u8>, Option<Bytes>>;

/// Bytes per pg_largeobject row (BLCKSZ / 4).
pub const LOBLKSIZE: i64 = 2048;
pub const MAX_LARGE_OBJECT_SIZE: i64 = i32::MAX as i64 * LOBLKSIZE;
/// lo_open modes (libpq-fs.h).
pub const INV_WRITE: i32 = 0x20000;
pub const INV_READ: i32 = 0x40000;
/// pg_largeobject's oid: the class of large objects in pg_depend, pg_description, ….
pub const CLASS_OID: u32 = 2613;

const META: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0x10];
const DATA: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0x11];

/// A pg_largeobject_metadata row (and the object's pg_description comment).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoMeta {
    pub owner: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acl: Option<Acl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// An open descriptor.
#[derive(Clone, Debug)]
pub struct Desc {
    pub oid: u32,
    pub offset: i64,
    pub read: bool,
    pub write: bool,
    /// Snapshot a read-only descriptor reads at; writers see the latest data.
    snapshot: Option<u64>,
    /// A read-only descriptor's view of our own writes to the object: those made before it
    /// was opened (later commands' aren't in its snapshot).
    own: Option<Arc<Writes>>,
    /// Subtransaction depth it was opened at: rolling that back closes it.
    pub subid: usize,
}

/// Which data a read sees.
#[derive(Clone, Copy, Debug)]
pub enum Snap {
    /// The statement's snapshot.
    Txn,
    /// The newest committed data (Postgres's catalog snapshot).
    Latest,
    At(u64),
}

fn meta_key(oid: u32) -> Vec<u8> {
    let mut k = META.to_vec();
    k.extend_from_slice(&oid.to_be_bytes());
    k
}

fn data_prefix(oid: u32) -> Vec<u8> {
    let mut k = DATA.to_vec();
    k.extend_from_slice(&oid.to_be_bytes());
    k
}

fn page_key(oid: u32, pageno: i32) -> Vec<u8> {
    let mut k = data_prefix(oid);
    k.extend_from_slice(&(pageno as u32).to_be_bytes());
    k
}

fn key_pageno(k: &[u8]) -> i32 {
    i32::from_be_bytes(k[12..16].try_into().unwrap())
}

fn ts(txn: &Txn, snap: Snap) -> u64 {
    match snap {
        Snap::Txn => txn.snapshot_ts(),
        Snap::Latest => txn.db.store.visible_ts(),
        Snap::At(t) => t,
    }
}

fn decode_meta(b: &[u8]) -> Result<LoMeta> {
    serde_json::from_slice(b)
        .map_err(|e| crate::error::internal(format!("corrupt large object metadata: {e}")))
}

fn does_not_exist(oid: u32) -> Error {
    Error::new(
        code::UNDEFINED_OBJECT,
        format!("large object {oid} does not exist"),
    )
}

/// The metadata of large object `oid`, if it exists.
pub fn meta(txn: &Txn, oid: u32, snap: Snap) -> Result<Option<LoMeta>> {
    match txn.get_at(&meta_key(oid), ts(txn, snap))? {
        Some(b) => Ok(Some(decode_meta(&b)?)),
        None => Ok(None),
    }
}

/// Like `meta`, but a missing object is an error.
pub fn existing(txn: &Txn, oid: u32, snap: Snap) -> Result<LoMeta> {
    meta(txn, oid, snap)?.ok_or_else(|| does_not_exist(oid))
}

/// Every large object, in oid order.
pub fn all(txn: &Txn) -> Result<Vec<(u32, LoMeta)>> {
    let start = META.to_vec();
    let mut out = Vec::new();
    for e in txn.scan(&start, &prefix_end(&start)) {
        let (k, v) = e?;
        out.push((
            u32::from_be_bytes(k[8..12].try_into().unwrap()),
            decode_meta(&v)?,
        ));
    }
    Ok(out)
}

/// Every pg_largeobject row: (loid, pageno, data).
pub fn all_pages(txn: &Txn) -> Result<Vec<(u32, i32, Bytes)>> {
    let start = DATA.to_vec();
    let mut out = Vec::new();
    for e in txn.scan(&start, &prefix_end(&start)) {
        let (k, v) = e?;
        out.push((
            u32::from_be_bytes(k[8..12].try_into().unwrap()),
            key_pageno(&k),
            v,
        ));
    }
    Ok(out)
}

/// Store new metadata for `oid`.
pub fn put_meta(txn: &mut Txn, oid: u32, m: &LoMeta) -> Result<()> {
    let blob = serde_json::to_vec(m).map_err(|e| crate::error::internal(e.to_string()))?;
    txn.put(meta_key(oid), blob)
}

/// Lock a key until the transaction ends. A committed change made meanwhile by the
/// transaction we waited for is a conflict.
fn lock_for_update(txn: &mut Txn, key: &[u8], conflict: impl FnOnce() -> Error) -> Result<()> {
    if txn.holds_lock(key) {
        return Ok(());
    }
    let before = txn.db.store.visible_ts();
    txn.lock(key, LockMode::Exclusive, WaitPolicy::Block, &|| {
        "large object".to_string()
    })?;
    if txn.latest_commit_ts(key)?.is_some_and(|t| t > before) {
        return Err(conflict());
    }
    Ok(())
}

/// The ACL in effect for a large object.
pub fn effective_acl(m: &LoMeta) -> Acl {
    m.acl
        .clone()
        .unwrap_or_else(|| acl::default_acl(ObjKind::LargeObject, m.owner))
}

/// (privileges, grant options) `role` holds on a large object.
pub fn mask(cat: &Catalog, m: &LoMeta, role: u32, db_owner: u32) -> (u32, u32) {
    let all = ObjKind::LargeObject.all();
    if cat.is_superuser(role) {
        return (all, all);
    }
    let (p, g) = cat.aclmask(&effective_acl(m), role, m.owner, db_owner);
    (p & all, g & all)
}

/// LargeObjectCreate: a new large object `oid` (0: pick one) owned by `owner`.
pub fn create(txn: &mut Txn, cat: &Catalog, oid: u32, owner: u32) -> Result<u32> {
    let oid = if oid != 0 {
        oid
    } else {
        // GetNewOidWithIndex: the next free oid of pg_largeobject_metadata.
        let mut o = txn.db.next_lo_oid(cat.next_oid);
        while o < crate::catalog::FIRST_USER_OID || meta(txn, o, Snap::Latest)?.is_some() {
            o = txn.db.next_lo_oid(o.wrapping_add(1));
        }
        o
    };
    let dup = || {
        Error::new(
            code::UNIQUE_VIOLATION,
            "duplicate key value violates unique constraint \"pg_largeobject_metadata_oid_index\"",
        )
        .detail(format!("Key (oid)=({oid}) already exists."))
        .table("pg_catalog", "pg_largeobject_metadata")
        .constraint("pg_largeobject_metadata_oid_index")
    };
    let key = meta_key(oid);
    lock_for_update(txn, &key, dup)?;
    if meta(txn, oid, Snap::Latest)?.is_some() {
        return Err(dup());
    }
    // The owner's default privileges for large objects (ALTER DEFAULT PRIVILEGES … ON LARGE OBJECTS).
    let acl = cat
        .default_acls
        .iter()
        .find(|d| d.objtype == 'L' && d.role == owner && d.schema == 0)
        .map(|d| d.acl.clone());
    put_meta(
        txn,
        oid,
        &LoMeta {
            owner,
            acl,
            comment: None,
        },
    )?;
    Ok(oid)
}

/// LargeObjectDrop: delete a large object's metadata and pages.
pub fn drop(txn: &mut Txn, oid: u32) -> Result<()> {
    let key = meta_key(oid);
    lock_for_update(txn, &key, || {
        Error::new(code::INTERNAL_ERROR, "tuple concurrently deleted")
    })?;
    if meta(txn, oid, Snap::Latest)?.is_none() {
        return Err(does_not_exist(oid));
    }
    txn.delete(key)?;
    let start = data_prefix(oid);
    let keys: Vec<Vec<u8>> = txn
        .scan_latest(&start, &prefix_end(&start))
        .map(|e| e.map(|(k, _)| k))
        .collect::<Result<_>>()?;
    for k in keys {
        txn.delete(k)?;
    }
    Ok(())
}

/// Pages of `oid` from `from` on, in order (at most `limit` page numbers ahead when given).
fn pages(
    txn: &Txn,
    oid: u32,
    from: i32,
    to: Option<i32>,
    at: u64,
    own: Option<&Writes>,
) -> Result<Vec<(i32, Bytes)>> {
    let start = page_key(oid, from);
    let end = match to {
        Some(t) if t < i32::MAX => page_key(oid, t + 1),
        _ => prefix_end(&data_prefix(oid)),
    };
    txn.scan_overlay(&start, &end, at, own)
        .map(|e| e.map(|(k, v)| (key_pageno(&k), v)))
        .collect()
}

fn corrupt(oid: u32, pageno: i32, len: usize) -> Error {
    Error::new(
        code::DATA_CORRUPTED,
        format!(
            "pg_largeobject entry for OID {oid}, page {pageno} has invalid data field size {len}"
        ),
    )
}

impl Desc {
    fn at(&self, txn: &Txn) -> u64 {
        self.snapshot.unwrap_or_else(|| txn.db.store.visible_ts())
    }

    fn denied(&self) -> Error {
        Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for large object {}", self.oid),
        )
    }

    /// inv_getsize: the end of the last page.
    fn size(&self, txn: &Txn) -> Result<i64> {
        let at = self.at(txn);
        // The last page: binary search for the highest page number with a page at or past it.
        let first = |p: i32| -> Result<Option<(i32, Bytes)>> {
            let start = page_key(self.oid, p);
            match txn
                .scan_overlay(
                    &start,
                    &prefix_end(&data_prefix(self.oid)),
                    at,
                    self.own.as_deref(),
                )
                .next()
            {
                Some(e) => e.map(|(k, v)| Some((key_pageno(&k), v))),
                None => Ok(None),
            }
        };
        let Some(mut best) = first(0)? else {
            return Ok(0);
        };
        let (mut lo, mut hi) = (best.0 as i64 + 1, i32::MAX as i64);
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            match first(mid as i32)? {
                Some(p) => {
                    lo = p.0 as i64 + 1;
                    best = p;
                }
                None => hi = mid - 1,
            }
        }
        if best.1.len() as i64 > LOBLKSIZE {
            return Err(corrupt(self.oid, best.0, best.1.len()));
        }
        Ok(best.0 as i64 * LOBLKSIZE + best.1.len() as i64)
    }

    /// inv_seek.
    pub fn seek(&mut self, txn: &Txn, offset: i64, whence: i32) -> Result<i64> {
        let new = match whence {
            0 => Some(offset),
            1 => self.offset.checked_add(offset),
            2 => self.size(txn)?.checked_add(offset),
            _ => {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("invalid whence setting: {whence}"),
                ));
            }
        };
        match new {
            Some(n) if (0..=MAX_LARGE_OBJECT_SIZE).contains(&n) => {
                self.offset = n;
                Ok(n)
            }
            _ => Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "invalid large object seek target: {}",
                    new.unwrap_or(if offset < 0 { i64::MIN } else { i64::MAX })
                ),
            )),
        }
    }

    /// inv_read: up to `n` bytes from the current offset (holes read as zeros).
    pub fn read(&mut self, txn: &Txn, n: i64) -> Result<Vec<u8>> {
        if !self.read {
            return Err(self.denied());
        }
        let mut out = Vec::new();
        if n <= 0 {
            return Ok(out);
        }
        let n = n as usize;
        let first = (self.offset / LOBLKSIZE) as i32;
        let last = ((self.offset + n as i64 - 1) / LOBLKSIZE).min(i32::MAX as i64) as i32;
        let at = self.at(txn);
        for (pageno, data) in pages(txn, self.oid, first, Some(last), at, self.own.as_deref())? {
            if data.len() as i64 > LOBLKSIZE {
                return Err(corrupt(self.oid, pageno, data.len()));
            }
            let pageoff = pageno as i64 * LOBLKSIZE;
            if pageoff > self.offset {
                let gap = ((pageoff - self.offset) as usize).min(n - out.len());
                out.resize(out.len() + gap, 0);
                self.offset += gap as i64;
            }
            if out.len() < n {
                let off = (self.offset - pageoff) as usize;
                if data.len() > off {
                    let take = (data.len() - off).min(n - out.len());
                    out.extend_from_slice(&data[off..off + take]);
                    self.offset += take as i64;
                }
            }
            if out.len() >= n {
                break;
            }
        }
        // The rest of the request is a hole if any page follows it.
        if out.len() < n
            && last < i32::MAX
            && txn
                .scan_overlay(
                    &page_key(self.oid, last + 1),
                    &prefix_end(&data_prefix(self.oid)),
                    at,
                    self.own.as_deref(),
                )
                .next()
                .transpose()?
                .is_some()
        {
            let gap = n - out.len();
            out.resize(n, 0);
            self.offset += gap as i64;
        }
        Ok(out)
    }

    /// inv_write: `buf` at the current offset.
    pub fn write(&mut self, txn: &mut Txn, buf: &[u8]) -> Result<i32> {
        if !self.write {
            return Err(self.denied());
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() as i64 + self.offset > MAX_LARGE_OBJECT_SIZE {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("invalid large object write request size: {}", buf.len()),
            ));
        }
        let first = (self.offset / LOBLKSIZE) as i32;
        let last = ((self.offset + buf.len() as i64 - 1) / LOBLKSIZE) as i32;
        let old: std::collections::HashMap<i32, Bytes> = pages(
            txn,
            self.oid,
            first,
            Some(last),
            txn.db.store.visible_ts(),
            None,
        )?
        .into_iter()
        .collect();
        let mut written = 0usize;
        let mut pageno = first;
        while written < buf.len() {
            let key = page_key(self.oid, pageno);
            let existed = old.get(&pageno);
            let (oid, p, had) = (self.oid, pageno, existed.is_some());
            let conflict = move || {
                if had {
                    Error::new(code::INTERNAL_ERROR, "tuple concurrently updated")
                } else {
                    Error::new(code::UNIQUE_VIOLATION, "duplicate key value violates unique constraint \"pg_largeobject_loid_pn_index\"")
                        .detail(format!("Key (loid, pageno)=({oid}, {p}) already exists."))
                        .table("pg_catalog", "pg_largeobject")
                        .constraint("pg_largeobject_loid_pn_index")
                }
            };
            lock_for_update(txn, &key, conflict)?;
            let mut page = match existed {
                Some(d) if d.len() as i64 > LOBLKSIZE => {
                    return Err(corrupt(self.oid, pageno, d.len()));
                }
                Some(d) => d.to_vec(),
                None => Vec::new(),
            };
            let off = (self.offset % LOBLKSIZE) as usize;
            if page.len() < off {
                page.resize(off, 0);
            }
            let n = (LOBLKSIZE as usize - off).min(buf.len() - written);
            if page.len() < off + n {
                page.resize(off + n, 0);
            }
            page[off..off + n].copy_from_slice(&buf[written..written + n]);
            written += n;
            self.offset += n as i64;
            txn.put(key, page)?;
            pageno += 1;
        }
        Ok(written as i32)
    }

    /// inv_truncate: make the object `len` bytes long.
    pub fn truncate(&mut self, txn: &mut Txn, len: i64) -> Result<()> {
        if !self.write {
            return Err(self.denied());
        }
        if !(0..=MAX_LARGE_OBJECT_SIZE).contains(&len) {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("invalid large object truncation target: {len}"),
            ));
        }
        let pageno = (len / LOBLKSIZE) as i32;
        let off = (len % LOBLKSIZE) as usize;
        let old = pages(txn, self.oid, pageno, None, txn.db.store.visible_ts(), None)?;
        let key = page_key(self.oid, pageno);
        lock_for_update(txn, &key, || {
            Error::new(code::INTERNAL_ERROR, "tuple concurrently updated")
        })?;
        let mut page = match old.first() {
            Some((p, d)) if *p == pageno => {
                if d.len() as i64 > LOBLKSIZE {
                    return Err(corrupt(self.oid, *p, d.len()));
                }
                d[..d.len().min(off)].to_vec()
            }
            _ => Vec::new(),
        };
        page.resize(off, 0);
        txn.put(key, page)?;
        for (p, _) in old.iter().filter(|(p, _)| *p > pageno) {
            let k = page_key(self.oid, *p);
            lock_for_update(txn, &k, || {
                Error::new(code::INTERNAL_ERROR, "tuple concurrently updated")
            })?;
            txn.delete(k)?;
        }
        Ok(())
    }
}

/// inv_open: check the object and the privileges a mode needs; the descriptor it would get.
/// `compat` is lo_compat_privileges (no privilege checks).
pub fn open(
    txn: &Txn,
    cat: &Catalog,
    oid: u32,
    mode: i32,
    role: u32,
    db_owner: u32,
    compat: bool,
) -> Result<Desc> {
    let write = mode & INV_WRITE != 0;
    let read = write || mode & INV_READ != 0;
    if !read {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("invalid flags for opening a large object: {mode}"),
        ));
    }
    let snapshot = if write { None } else { Some(txn.snapshot_ts()) };
    let snap = snapshot.map_or(Snap::Latest, Snap::At);
    let m = existing(txn, oid, snap)?;
    if !compat {
        let (held, _) = mask(cat, &m, role, db_owner);
        let need = if write {
            acl::SELECT | acl::UPDATE
        } else {
            acl::SELECT
        };
        if held & need != need {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for large object {oid}"),
            ));
        }
    }
    let own = match write {
        false => {
            let start = data_prefix(oid);
            Some(Arc::new(txn.writes_in(&start, &prefix_end(&start))?))
        }
        true => None,
    };
    Ok(Desc {
        oid,
        offset: 0,
        read,
        write,
        snapshot,
        own,
        subid: txn.subtxn_depth(),
    })
}

impl Txn {
    /// newLOfd: the lowest free descriptor number.
    pub fn lo_add(&mut self, d: Desc) -> i32 {
        match self.lo_descs.iter().position(Option::is_none) {
            Some(i) => {
                self.lo_descs[i] = Some(d);
                i as i32
            }
            None => {
                self.lo_descs.push(Some(d));
                self.lo_descs.len() as i32 - 1
            }
        }
    }

    pub fn lo_desc(&mut self, fd: i32) -> Result<&mut Desc> {
        match usize::try_from(fd)
            .ok()
            .and_then(|i| self.lo_descs.get_mut(i))
            .and_then(Option::as_mut)
        {
            Some(d) => Ok(d),
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("invalid large-object descriptor: {fd}"),
            )),
        }
    }

    pub fn lo_close(&mut self, fd: i32) -> Result<()> {
        self.lo_desc(fd)?;
        self.lo_descs[fd as usize] = None;
        Ok(())
    }

    /// Close every descriptor of large object `oid` (it's being unlinked).
    pub fn lo_close_all(&mut self, oid: u32) {
        for d in &mut self.lo_descs {
            if d.as_ref().is_some_and(|d| d.oid == oid) {
                *d = None;
            }
        }
    }
}
