//! Rows and the copy-on-write rules, inside a transaction.
//!
//! A bucket's view of a key: its own row (a tombstone means "absent"), else its parent's view,
//! recursively. Writes to a bucket push the old view down to children that don't have their
//! own row first, so a branch's view never changes under it.
//!
//! `blobs.refs` counts every `objects`, `versions` and `trash` row pointing at the blob. All
//! changes to it go through [`add_ref`], which refuses to go below zero or to revive an
//! unreferenced blob: either would mean a bookkeeping bug, and failing the write beats losing
//! data.
//!
//! A row is a live version (it has a blob), a delete marker (no blob, `delete_marker`), or a
//! tombstone (neither: "absent here", only in branches). `objects` holds each key's current
//! row; `versions` the noncurrent ones (see `versioning`).

use crate::error::{Result, StoreError};
use crate::model::{Checksum, ObjectHeaders, ObjectLock, ObjectMeta};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub key: String,
    /// `None`: a tombstone or a delete marker.
    pub blob_id: Option<String>,
    pub size: u64,
    pub etag: String,
    pub headers: ObjectHeaders,
    pub checksum: Option<Checksum>,
    pub parts: u32,
    pub part_sizes: Vec<u64>,
    /// The parts' checksums (in the object's checksum algorithm), when every part had one;
    /// empty otherwise. Kept in the `part_sizes` column (see [`PartsCol`]).
    pub part_checksums: Vec<String>,
    pub modified_at: i64,
    /// Version id; sorts newest first within a key. Empty for tombstones.
    pub vid: String,
    /// The "null" version (unversioned and suspended writes).
    pub is_null: bool,
    pub delete_marker: bool,
    pub lock: ObjectLock,
}

impl Row {
    pub fn tombstone(key: &str, now: i64) -> Self {
        Self {
            key: key.to_string(),
            blob_id: None,
            size: 0,
            etag: String::new(),
            headers: ObjectHeaders::default(),
            checksum: None,
            parts: 0,
            part_sizes: Vec::new(),
            part_checksums: Vec::new(),
            modified_at: now,
            vid: String::new(),
            is_null: true,
            delete_marker: false,
            lock: ObjectLock::default(),
        }
    }

    pub fn marker(key: &str, vid: String, is_null: bool, now: i64) -> Self {
        Self { vid, is_null, delete_marker: true, ..Self::tombstone(key, now) }
    }

    pub fn is_tombstone(&self) -> bool {
        self.blob_id.is_none() && !self.delete_marker
    }

    /// The id S3 shows: `null` or the vid.
    pub fn version_id(&self) -> String {
        if self.is_null { "null".to_string() } else { self.vid.clone() }
    }

    /// Whether this is the version S3 calls `version_id`.
    pub fn is_version(&self, version_id: &str) -> bool {
        if version_id == "null" { self.is_null } else { !self.is_null && self.vid == version_id }
    }

    pub fn blob(&self) -> Result<&str> {
        self.blob_id.as_deref().ok_or_else(|| StoreError::internal("tombstone used as an object"))
    }

    pub fn meta(&self) -> Result<ObjectMeta> {
        Ok(ObjectMeta {
            key: self.key.clone(),
            blob_id: self.blob()?.to_string(),
            size: self.size,
            etag: self.etag.clone(),
            headers: self.headers.clone(),
            checksum: self.checksum.clone(),
            parts: self.parts,
            part_sizes: self.part_sizes.clone(),
            part_checksums: self.part_checksums.clone(),
            modified_at: self.modified_at,
            version_id: self.version_id(),
            lock: self.lock.clone(),
        })
    }
}

/// Object columns after `bucket_id` (the trash adds `deleted_at` before them, `versions`
/// `noncurrent_at`).
pub(crate) const COLS: &str = "key, blob_id, size, etag, content_type, cache_control, content_disposition, content_encoding, \
     content_language, expires, metadata, tags, checksum_algorithm, checksum_type, checksum, parts, part_sizes, modified_at, \
     acl, website_redirect, vid, is_null, delete_marker, lock_mode, lock_until, legal_hold, owner";

fn json_col<T: serde::de::DeserializeOwned>(r: &rusqlite::Row, i: usize) -> rusqlite::Result<T> {
    let s: String = r.get(i)?;
    serde_json::from_str(&s).map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e)))
}

/// The `part_sizes` column: the parts' sizes (`[n, …]`, as it always was), or, for an object
/// whose parts' checksums are kept, `{"sizes": [n, …], "checksums": ["…", …]}`. Rows without
/// part checksums keep the old form, so they're written exactly as before.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum PartsCol {
    Sizes(Vec<u64>),
    WithChecksums {
        sizes: Vec<u64>,
        #[serde(default)]
        checksums: Vec<String>,
    },
}

fn parts_col(r: &rusqlite::Row, i: usize) -> rusqlite::Result<(Vec<u64>, Vec<String>)> {
    Ok(match json_col(r, i)? {
        PartsCol::Sizes(s) => (s, Vec::new()),
        // Never trust a list that doesn't line up with the sizes.
        PartsCol::WithChecksums { sizes, checksums } if checksums.len() == sizes.len() => (sizes, checksums),
        PartsCol::WithChecksums { sizes, .. } => (sizes, Vec::new()),
    })
}

fn parts_text(r: &Row) -> serde_json::Result<String> {
    if r.part_checksums.is_empty() {
        serde_json::to_string(&r.part_sizes)
    } else {
        serde_json::to_string(&PartsCol::WithChecksums { sizes: r.part_sizes.clone(), checksums: r.part_checksums.clone() })
    }
}

/// A row from `SELECT {COLS}` starting at column `o`.
/// How many columns [`COLS`] has: the first extra column after them is at this index.
pub(crate) const COLS_LEN: usize = 27;

pub(crate) fn row_at(r: &rusqlite::Row, o: usize) -> rusqlite::Result<Row> {
    let algorithm: Option<String> = r.get(o + 12)?;
    let kind: Option<String> = r.get(o + 13)?;
    let value: Option<String> = r.get(o + 14)?;
    let (part_sizes, part_checksums) = parts_col(r, o + 16)?;
    Ok(Row {
        key: r.get(o)?,
        blob_id: r.get(o + 1)?,
        size: r.get::<_, i64>(o + 2)? as u64,
        etag: r.get(o + 3)?,
        headers: ObjectHeaders {
            content_type: r.get(o + 4)?,
            cache_control: r.get(o + 5)?,
            content_disposition: r.get(o + 6)?,
            content_encoding: r.get(o + 7)?,
            content_language: r.get(o + 8)?,
            expires: r.get(o + 9)?,
            metadata: json_col(r, o + 10)?,
            tags: json_col(r, o + 11)?,
            acl: r.get(o + 18)?,
            website_redirect: r.get(o + 19)?,
            owner: r.get(o + 26)?,
        },
        checksum: match (algorithm, kind, value) {
            (Some(algorithm), Some(kind), Some(value)) => Some(Checksum { algorithm, kind, value }),
            _ => None,
        },
        parts: r.get::<_, i64>(o + 15)? as u32,
        part_sizes,
        part_checksums,
        modified_at: r.get(o + 17)?,
        vid: r.get(o + 20)?,
        is_null: r.get::<_, i64>(o + 21)? != 0,
        delete_marker: r.get::<_, i64>(o + 22)? != 0,
        lock: ObjectLock { mode: r.get(o + 23)?, retain_until: r.get(o + 24)?, legal_hold: r.get::<_, Option<i64>>(o + 25)?.map(|v| v != 0) },
    })
}

fn insert_sql(table: &str, extra: &str) -> String {
    let n = COLS.split(',').count() + 1 + usize::from(!extra.is_empty());
    let marks: Vec<String> = (1..=n).map(|i| format!("?{i}")).collect();
    let extra_col = if extra.is_empty() { String::new() } else { format!("{extra}, ") };
    format!("INSERT INTO {table} (bucket_id, {extra_col}{COLS}) VALUES ({})", marks.join(", "))
}

fn row_values(r: &Row) -> Result<Vec<rusqlite::types::Value>> {
    use rusqlite::types::Value::{Integer, Null, Text};
    let opt = |v: &Option<String>| v.clone().map_or(Null, Text);
    Ok(vec![
        Text(r.key.clone()),
        opt(&r.blob_id),
        Integer(r.size as i64),
        Text(r.etag.clone()),
        opt(&r.headers.content_type),
        opt(&r.headers.cache_control),
        opt(&r.headers.content_disposition),
        opt(&r.headers.content_encoding),
        opt(&r.headers.content_language),
        opt(&r.headers.expires),
        Text(serde_json::to_string(&r.headers.metadata)?),
        Text(serde_json::to_string(&r.headers.tags)?),
        opt(&r.checksum.as_ref().map(|c| c.algorithm.clone())),
        opt(&r.checksum.as_ref().map(|c| c.kind.clone())),
        opt(&r.checksum.as_ref().map(|c| c.value.clone())),
        Integer(r.parts as i64),
        Text(parts_text(r)?),
        Integer(r.modified_at),
        opt(&r.headers.acl),
        opt(&r.headers.website_redirect),
        Text(r.vid.clone()),
        Integer(r.is_null as i64),
        Integer(r.delete_marker as i64),
        opt(&r.lock.mode),
        r.lock.retain_until.map_or(Null, Integer),
        r.lock.legal_hold.map_or(Null, |h| Integer(h as i64)),
        opt(&r.headers.owner),
    ])
}

/// The bucket and its ancestors, nearest first.
pub(crate) fn chain(c: &Connection, bucket_id: &str) -> Result<Vec<String>> {
    let mut out = vec![bucket_id.to_string()];
    let mut cur = bucket_id.to_string();
    while let Some(p) = c.query_row("SELECT parent_id FROM buckets WHERE id = ?1", [&cur], |r| r.get::<_, Option<String>>(0)).optional()?.flatten() {
        if out.contains(&p) || out.len() > 1000 {
            return Err(StoreError::internal(format!("bucket ancestry loops at {p}")));
        }
        out.push(p.clone());
        cur = p;
    }
    Ok(out)
}

pub(crate) fn children(c: &Connection, bucket_id: &str) -> Result<Vec<String>> {
    let mut st = c.prepare_cached("SELECT id FROM buckets WHERE parent_id = ?1 ORDER BY id")?;
    let rows = st.query_map([bucket_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(rows)
}

pub(crate) fn own_row(c: &Connection, bucket_id: &str, key: &str) -> Result<Option<Row>> {
    let mut st = c.prepare_cached(&format!("SELECT {COLS} FROM objects WHERE bucket_id = ?1 AND key = ?2"))?;
    Ok(st.query_row(params![bucket_id, key], |r| row_at(r, 0)).optional()?)
}

/// The visible row of `key` in the first bucket of `chain`, and the index of the bucket that
/// owns it.
pub(crate) fn resolve(c: &Connection, chain: &[String], key: &str) -> Result<Option<(Row, usize)>> {
    for (i, b) in chain.iter().enumerate() {
        if let Some(r) = own_row(c, b, key)? {
            return Ok(if r.blob_id.is_some() { Some((r, i)) } else { None });
        }
    }
    Ok(None)
}

/// Like [`resolve`], but a delete marker counts: the key's current version, live or a marker.
pub(crate) fn resolve_current(c: &Connection, chain: &[String], key: &str) -> Result<Option<(Row, usize)>> {
    for (i, b) in chain.iter().enumerate() {
        if let Some(r) = own_row(c, b, key)? {
            return Ok(if r.is_tombstone() { None } else { Some((r, i)) });
        }
    }
    Ok(None)
}

/// The bucket's view of `key` is about to change: children without their own row keep the old
/// view (a copy of its current version, or a tombstone if it had none).
pub(crate) fn push_down(c: &Connection, bucket_id: &str, chain: &[String], key: &str, now: i64) -> Result<()> {
    let kids = children(c, bucket_id)?;
    if kids.is_empty() {
        return Ok(());
    }
    let old = resolve_current(c, chain, key)?;
    for child in kids {
        if own_row(c, &child, key)?.is_none() {
            match &old {
                Some((r, _)) => {
                    if let Some(b) = &r.blob_id {
                        add_ref(c, b, 1, now)?;
                    }
                    insert_row(c, &child, r)?;
                }
                None => insert_row(c, &child, &Row::tombstone(key, now))?,
            }
        }
    }
    Ok(())
}

/// Noncurrent versions of `key` the bucket owns, newest first, with when each became noncurrent.
pub(crate) fn own_versions(c: &Connection, bucket_id: &str, key: &str) -> Result<Vec<(Row, i64)>> {
    let mut st = c.prepare_cached(&format!("SELECT {COLS}, noncurrent_at FROM versions WHERE bucket_id = ?1 AND key = ?2 ORDER BY vid"))?;
    let rows = st.query_map(params![bucket_id, key], |r| Ok((row_at(r, 0)?, r.get(COLS_LEN)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Add a noncurrent version (references are the caller's business).
pub(crate) fn version_insert(c: &Connection, bucket_id: &str, r: &Row, noncurrent_at: i64) -> Result<()> {
    let mut values = vec![rusqlite::types::Value::Text(bucket_id.to_string()), rusqlite::types::Value::Integer(noncurrent_at)];
    values.extend(row_values(r)?);
    c.prepare_cached(&insert_sql("versions", "noncurrent_at"))?.execute(rusqlite::params_from_iter(values))?;
    c.prepare_cached("UPDATE buckets SET versions = versions + 1, version_bytes = version_bytes + ?2 WHERE id = ?1")?.execute(params![bucket_id, r.size as i64])?;
    Ok(())
}

/// Remove a noncurrent version (references are the caller's business).
pub(crate) fn version_delete(c: &Connection, bucket_id: &str, r: &Row) -> Result<()> {
    let n = c.prepare_cached("DELETE FROM versions WHERE bucket_id = ?1 AND key = ?2 AND vid = ?3")?.execute(params![bucket_id, r.key, r.vid])?;
    if n != 1 {
        return Err(StoreError::internal(format!("noncurrent version {} of {} vanished", r.vid, r.key)));
    }
    c.prepare_cached("UPDATE buckets SET versions = versions - 1, version_bytes = version_bytes - ?2 WHERE id = ?1")?.execute(params![bucket_id, r.size as i64])?;
    Ok(())
}

fn counters(c: &Connection, bucket_id: &str, objects: i64, bytes: i64) -> Result<()> {
    if objects != 0 || bytes != 0 {
        c.execute("UPDATE buckets SET objects = objects + ?2, bytes = bytes + ?3 WHERE id = ?1", params![bucket_id, objects, bytes])?;
    }
    Ok(())
}

/// Insert a row the bucket doesn't have yet. References are the caller's business.
pub(crate) fn insert_row(c: &Connection, bucket_id: &str, r: &Row) -> Result<()> {
    let mut values = vec![rusqlite::types::Value::Text(bucket_id.to_string())];
    values.extend(row_values(r)?);
    c.prepare_cached(&insert_sql("objects", ""))?.execute(rusqlite::params_from_iter(values))?;
    if r.blob_id.is_some() {
        counters(c, bucket_id, 1, r.size as i64)?;
    }
    Ok(())
}

/// Remove the bucket's own row (references are the caller's business).
pub(crate) fn delete_own_row(c: &Connection, bucket_id: &str, key: &str) -> Result<Option<Row>> {
    let Some(r) = own_row(c, bucket_id, key)? else { return Ok(None) };
    c.prepare_cached("DELETE FROM objects WHERE bucket_id = ?1 AND key = ?2")?.execute(params![bucket_id, key])?;
    if r.blob_id.is_some() {
        counters(c, bucket_id, -1, -(r.size as i64))?;
    }
    Ok(Some(r))
}

/// Change a blob's reference count.
pub(crate) fn add_ref(c: &Connection, blob: &str, delta: i64, now: i64) -> Result<()> {
    let refs: Option<i64> = c.prepare_cached("SELECT refs FROM blobs WHERE id = ?1")?.query_row([blob], |r| r.get(0)).optional()?;
    let Some(refs) = refs else { return Err(StoreError::internal(format!("blob {blob} has no metadata"))) };
    if refs == 0 && delta > 0 {
        return Err(StoreError::internal(format!("blob {blob} is unreferenced and may be deleted; refusing to reference it again")));
    }
    let n = refs + delta;
    if n < 0 {
        return Err(StoreError::internal(format!("blob {blob} would have {n} references")));
    }
    c.prepare_cached("UPDATE blobs SET refs = ?2, unreferenced_at = CASE WHEN ?2 = 0 THEN ?3 ELSE NULL END WHERE id = ?1")?.execute(params![blob, n, now])?;
    Ok(())
}

pub(crate) fn insert_blob(c: &Connection, blob: &str, size: u64, crc32c: u32, now: i64) -> Result<()> {
    c.prepare_cached("INSERT INTO blobs (id, size, crc32c, refs, created_at, unreferenced_at) VALUES (?1, ?2, ?3, 1, ?4, NULL)")?
        .execute(params![blob, size as i64, crc32c as i64, now])?;
    Ok(())
}

/// Put a version into the bucket's trash (the caller moves or adds its reference).
pub(crate) fn trash_insert(c: &Connection, bucket_id: &str, r: &Row, now: i64) -> Result<i64> {
    let mut at = now;
    while c.prepare_cached("SELECT 1 FROM trash WHERE bucket_id = ?1 AND key = ?2 AND deleted_at = ?3")?.exists(params![bucket_id, r.key, at])? {
        at += 1;
    }
    let mut values = vec![rusqlite::types::Value::Text(bucket_id.to_string()), rusqlite::types::Value::Integer(at)];
    values.extend(row_values(r)?);
    c.prepare_cached(&insert_sql("trash", "deleted_at"))?.execute(rusqlite::params_from_iter(values))?;
    c.execute("UPDATE buckets SET trash_bytes = trash_bytes + ?2 WHERE id = ?1", params![bucket_id, r.size as i64])?;
    Ok(at)
}

pub(crate) fn trash_row(c: &Connection, bucket_id: &str, key: &str, deleted_at: i64) -> Result<Option<Row>> {
    Ok(c.prepare_cached(&format!("SELECT {COLS} FROM trash WHERE bucket_id = ?1 AND key = ?2 AND deleted_at = ?3"))?
        .query_row(params![bucket_id, key, deleted_at], |r| row_at(r, 0))
        .optional()?)
}

/// Remove a trash row (the caller moves or drops its reference).
pub(crate) fn trash_delete(c: &Connection, bucket_id: &str, key: &str, deleted_at: i64, size: u64) -> Result<()> {
    c.prepare_cached("DELETE FROM trash WHERE bucket_id = ?1 AND key = ?2 AND deleted_at = ?3")?.execute(params![bucket_id, key, deleted_at])?;
    c.execute("UPDATE buckets SET trash_bytes = trash_bytes - ?2 WHERE id = ?1", params![bucket_id, size as i64])?;
    Ok(())
}

/// The bucket's view of `key` changes from what it is to `new` (`None`: deleted). Children
/// keep their view; the old version goes to the trash if `trash` (and the bucket keeps one).
/// `new`'s blob reference is the caller's. Returns the old view.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_key(c: &Connection, bucket_id: &str, chain: &[String], recover_days: u32, key: &str, new: Option<&Row>, trash: bool, now: i64) -> Result<Option<Row>> {
    let old = resolve(c, chain, key)?;
    if new.is_none() && old.is_none() {
        return Ok(None);
    }
    // Dropping a locked version for good (no trash to keep it in) isn't allowed; checked before
    // anything changes.
    if let Some((r, 0)) = &old
        && !(trash && recover_days > 0)
        && r.lock.locked(now)
    {
        return Err(StoreError::ObjectLocked(key.to_string()));
    }
    push_down(c, bucket_id, chain, key, now)?;
    if let Some((r, owner)) = &old {
        let owned = *owner == 0;
        if trash && recover_days > 0 {
            if !owned {
                add_ref(c, r.blob()?, 1, now)?;
            }
            trash_insert(c, bucket_id, r, now)?;
            if owned {
                delete_own_row(c, bucket_id, key)?;
            }
        } else if owned {
            delete_own_row(c, bucket_id, key)?;
            add_ref(c, r.blob()?, -1, now)?;
        }
    }
    // What's left of the bucket's own row is a tombstone.
    if let Some(own) = delete_own_row(c, bucket_id, key)?
        && own.blob_id.is_some()
    {
        return Err(StoreError::internal(format!("unexpected live row for {key}")));
    }
    match new {
        Some(r) => insert_row(c, bucket_id, r)?,
        None => {
            if chain.len() > 1 && resolve(c, &chain[1..], key)?.is_some() {
                insert_row(c, bucket_id, &Row::tombstone(key, now))?;
            }
        }
    }
    Ok(old.map(|(r, _)| r))
}


/// A key past every key starting with `prefix` (keys are at most 1024 bytes, so 256 copies of
/// the largest code point sort after any of them).
pub(crate) fn past_prefix(prefix: &str) -> String {
    let mut s = String::with_capacity(prefix.len() + 1024);
    s.push_str(prefix);
    for _ in 0..256 {
        s.push(char::MAX);
    }
    s
}

/// The common prefix `key` rolls up into, if any.
pub(crate) fn common_prefix<'a>(key: &'a str, prefix: &str, delimiter: Option<&str>) -> Option<&'a str> {
    let d = delimiter.filter(|d| !d.is_empty())?;
    let rest = key.get(prefix.len()..)?;
    rest.find(d).map(|i| &key[..prefix.len() + i + d.len()])
}

struct Cursor {
    bucket: String,
    buf: VecDeque<Row>,
    from: String,
    inclusive: bool,
    done: bool,
}

const PAGE: usize = 256;

impl Cursor {
    fn fill(&mut self, c: &Connection, prefix: &str) -> Result<()> {
        if !self.buf.is_empty() || self.done {
            return Ok(());
        }
        let op = if self.inclusive { ">=" } else { ">" };
        let mut st = c.prepare_cached(&format!("SELECT {COLS} FROM objects WHERE bucket_id = ?1 AND key {op} ?2 ORDER BY key LIMIT ?3"))?;
        let rows = st.query_map(params![self.bucket, self.from, PAGE as i64], |r| row_at(r, 0))?.collect::<rusqlite::Result<Vec<Row>>>()?;
        if rows.len() < PAGE {
            self.done = true;
        }
        if let Some(last) = rows.last() {
            self.from = last.key.clone();
            self.inclusive = false;
        }
        for r in rows {
            if !r.key.starts_with(prefix) {
                self.done = true;
                break;
            }
            self.buf.push_back(r);
        }
        Ok(())
    }

    fn seek(&mut self, from: &str) {
        self.buf.clear();
        self.from = from.to_string();
        self.inclusive = true;
        self.done = false;
    }
}

pub(crate) enum Entry {
    Object(Box<Row>),
    Prefix(String),
}

/// Lists a bucket's view in key order: its rows merged with its ancestors' (nearer buckets
/// win, tombstones hide), with keys rolled up into common prefixes.
pub(crate) struct Merger<'a> {
    c: &'a Connection,
    cursors: Vec<Cursor>,
    prefix: String,
    delimiter: Option<String>,
    /// Yield delete markers too (version listings).
    markers: bool,
}

impl<'a> Merger<'a> {
    /// Current versions (delete markers included) from `from` on, no roll-up: the version
    /// listing does its own.
    pub(crate) fn current(c: &'a Connection, chain: &[String], prefix: &str, from: &str, inclusive: bool) -> Self {
        let cursors = chain.iter().map(|b| Cursor { bucket: b.clone(), buf: VecDeque::new(), from: from.to_string(), inclusive, done: false }).collect();
        Self { c, cursors, prefix: prefix.to_string(), delimiter: None, markers: true }
    }

    /// Continue from `from` (inclusive), e.g. past a common prefix.
    pub(crate) fn seek(&mut self, from: &str) {
        for cur in &mut self.cursors {
            cur.seek(from);
        }
    }

    pub(crate) fn new(c: &'a Connection, chain: &[String], prefix: &str, delimiter: Option<&str>, start_after: Option<&str>) -> Self {
        let delimiter = delimiter.filter(|d| !d.is_empty()).map(str::to_string);
        let (from, inclusive) = match start_after {
            Some(s) if s >= prefix => match common_prefix(s, prefix, delimiter.as_deref()) {
                // Resuming after a common prefix skips everything in it.
                Some(cp) if cp == s => (past_prefix(s), true),
                _ => (s.to_string(), false),
            },
            _ => (prefix.to_string(), true),
        };
        let cursors = chain.iter().map(|b| Cursor { bucket: b.clone(), buf: VecDeque::new(), from: from.clone(), inclusive, done: false }).collect();
        Self { c, cursors, prefix: prefix.to_string(), delimiter, markers: false }
    }

    pub(crate) fn next_entry(&mut self) -> Result<Option<Entry>> {
        loop {
            for cur in &mut self.cursors {
                cur.fill(self.c, &self.prefix)?;
            }
            let Some(key) = self.cursors.iter().filter_map(|c| c.buf.front().map(|r| &r.key)).min().cloned() else { return Ok(None) };
            let mut winner = None;
            for cur in &mut self.cursors {
                if let Some(r) = cur.buf.pop_front_if(|r| r.key == key)
                    && winner.is_none()
                {
                    winner = Some(r);
                }
            }
            let row = winner.expect("some cursor had the key");
            if row.is_tombstone() || (row.delete_marker && !self.markers) {
                continue;
            }
            if let Some(cp) = common_prefix(&row.key, &self.prefix, self.delimiter.as_deref()) {
                let cp = cp.to_string();
                let past = past_prefix(&cp);
                for cur in &mut self.cursors {
                    cur.seek(&past);
                }
                return Ok(Some(Entry::Prefix(cp)));
            }
            return Ok(Some(Entry::Object(Box::new(row))));
        }
    }
}

pub(crate) fn json<T: serde::Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cols_len_matches_cols() {
        assert_eq!(COLS.split(',').count(), COLS_LEN);
    }

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&c, crate::schema::LATEST).unwrap();
        c
    }

    #[test]
    fn refs_never_go_negative_and_a_dead_blob_stays_dead() {
        let c = db();
        insert_blob(&c, "b1", 10, 0, 100).unwrap();
        assert!(insert_blob(&c, "b1", 10, 0, 100).is_err(), "blob ids are never reused");
        let refs = |c: &Connection| c.query_row("SELECT refs, unreferenced_at FROM blobs WHERE id = 'b1'", [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))).unwrap();
        assert_eq!(refs(&c), (1, None));
        add_ref(&c, "b1", 2, 200).unwrap();
        assert_eq!(refs(&c), (3, None));
        assert!(add_ref(&c, "b1", -4, 300).is_err());
        assert_eq!(refs(&c), (3, None), "a refused change changes nothing");
        add_ref(&c, "b1", -3, 400).unwrap();
        assert_eq!(refs(&c), (0, Some(400)), "the quarantine starts at the last reference");
        let err = add_ref(&c, "b1", 1, 500).unwrap_err().to_string();
        assert!(err.contains("refusing"), "{err}");
        assert!(add_ref(&c, "b1", -1, 500).is_err());
        assert_eq!(refs(&c), (0, Some(400)));
        assert!(add_ref(&c, "nope", 1, 0).unwrap_err().to_string().contains("no metadata"));
    }

    #[test]
    fn ancestry_loops_are_refused() {
        let c = db();
        for (id, parent) in [("a", None), ("b", Some("a")), ("x", Some("y")), ("y", Some("x"))] {
            c.execute(
                "INSERT INTO buckets (id, name, region, created_at, public, recover_days, parent_id, parent_at, objects, bytes, trash_bytes) VALUES (?1, ?1, 'r', 0, 0, 7, ?2, 0, 0, 0, 0)",
                params![id, parent],
            )
            .unwrap();
        }
        assert_eq!(chain(&c, "b").unwrap(), ["b", "a"]);
        assert_eq!(children(&c, "a").unwrap(), ["b"]);
        assert!(chain(&c, "x").unwrap_err().to_string().contains("loops"));
    }

    #[test]
    fn write_key_refuses_to_bury_a_live_row() {
        let c = db();
        c.execute("INSERT INTO buckets (id, name, region, created_at, public, recover_days, objects, bytes, trash_bytes) VALUES ('a', 'a', 'r', 0, 0, 0, 0, 0, 0)", []).unwrap();
        insert_blob(&c, "b1", 1, 0, 0).unwrap();
        let mut r = Row::tombstone("k", 0);
        r.blob_id = Some("b1".into());
        r.size = 1;
        insert_row(&c, "a", &r).unwrap();
        // A chain that doesn't start with the bucket (a caller bug) must not orphan its row.
        assert!(write_key(&c, "a", &["elsewhere".into()], 0, "k", None, true, 1).is_ok(), "nothing visible: nothing to do");
        let err = write_key(&c, "a", &["elsewhere".into()], 0, "k", Some(&Row { blob_id: Some("b1".into()), ..Row::tombstone("k", 1) }), true, 1).unwrap_err();
        assert!(err.to_string().contains("unexpected live row"), "{err}");
    }

    #[test]
    fn prefixes() {
        assert_eq!(common_prefix("a/b/c", "", Some("/")), Some("a/"));
        assert_eq!(common_prefix("a/b/c", "a/", Some("/")), Some("a/b/"));
        assert_eq!(common_prefix("a/b", "a/", Some("/")), None);
        assert_eq!(common_prefix("a--b--c", "", Some("--")), Some("a--"));
        assert_eq!(common_prefix("a/b", "", Some("")), None);
        assert_eq!(common_prefix("a/b", "", None), None);
        // Past every key under the prefix, even the longest with the highest code points.
        let longest = format!("p/{}", "\u{10FFFF}".repeat(255));
        assert!(longest.len() <= 1024);
        assert!(past_prefix("p/") > longest);
        assert!(past_prefix("p/").as_str() < "p0");
    }
}
