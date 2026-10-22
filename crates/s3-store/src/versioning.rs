//! Versioning inside a transaction: version ids, noncurrent versions, delete markers, Object
//! Lock and how they fit the trash and branches. Builds on the copy-on-write rules in `rows`.
//!
//! - `objects` holds each key's current version (live, or a delete marker); `versions` the
//!   noncurrent ones, newest first by `vid`. A bucket only has noncurrent versions of a key
//!   whose current version it owns.
//! - Unversioned buckets keep today's behaviour: an overwrite or delete moves the old version
//!   to the trash ([`rows::write_key`]).
//! - Versioning `Enabled`: overwrites and deletes make the old version noncurrent (a delete adds
//!   a marker). `Suspended`: new writes are the null version, which replaces the previous null
//!   version wherever it is in the history.
//! - A permanent delete (a version by id, lifecycle noncurrent expiry, a replaced null version)
//!   moves the version to the trash for `recover_days`, so even those are recoverable. Delete
//!   markers carry no data and aren't kept.
//! - A version under Object Lock (an unexpired retention or a legal hold) is never deleted for
//!   good; only a GOVERNANCE retention can be bypassed, explicitly.

use crate::error::{Result, StoreError};
use crate::ids;
use crate::model::*;
use crate::rows::{self, Row};
use crate::store::BucketRow;
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) const DAY_MS: i64 = 86_400_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Enabled,
    Suspended,
}

impl Mode {
    pub fn of(b: &BucketRow) -> Self {
        match b.versioning.as_deref() {
            Some("Enabled") => Self::Enabled,
            Some("Suspended") => Self::Suspended,
            _ => Self::Off,
        }
    }
}

/// Where a version is in a bucket's view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// The current version, owned by `chain[i]` (0: the bucket itself).
    Current(usize),
    /// A noncurrent version the bucket owns.
    Noncurrent,
}

/// A version id for a new version of `key` that sorts before (is newer than) every version the
/// bucket has or sees of it, even if the clock went back or two writes share a millisecond.
pub(crate) fn new_vid(c: &Connection, bucket_id: &str, chain: &[String], key: &str, now: i64) -> Result<String> {
    let vid = ids::version_id(now);
    let mut newest = rows::resolve_current(c, chain, key)?.map(|(r, _)| r.vid).filter(|v| !v.is_empty());
    let own: Option<String> = c.prepare_cached("SELECT vid FROM versions WHERE bucket_id = ?1 AND key = ?2 ORDER BY vid LIMIT 1")?.query_row(params![bucket_id, key], |r| r.get(0)).optional()?;
    if let Some(o) = own
        && newest.as_ref().is_none_or(|n| o < *n)
    {
        newest = Some(o);
    }
    match newest {
        Some(n) if vid >= n => {
            let prefix = n.get(..16).and_then(|p| u64::from_str_radix(p, 16).ok()).ok_or_else(|| StoreError::internal(format!("malformed version id {n}")))?;
            let prefix = prefix.checked_sub(1).ok_or_else(|| StoreError::internal("version ids exhausted"))?;
            Ok(ids::version_id_with_prefix(prefix))
        }
        _ => Ok(vid),
    }
}

/// The bucket's own noncurrent version of `key` that S3 calls `version_id`.
fn own_version(c: &Connection, bucket_id: &str, key: &str, version_id: &str) -> Result<Option<Row>> {
    let sql = if version_id == "null" {
        format!("SELECT {} FROM versions WHERE bucket_id = ?1 AND key = ?2 AND is_null = 1 AND ?3 = 'null' ORDER BY vid LIMIT 1", rows::COLS)
    } else {
        format!("SELECT {} FROM versions WHERE bucket_id = ?1 AND key = ?2 AND vid = ?3 AND is_null = 0", rows::COLS)
    };
    Ok(c.prepare_cached(&sql)?.query_row(params![bucket_id, key, version_id], |r| rows::row_at(r, 0)).optional()?)
}

/// Find a version in the bucket's view: its own current row and noncurrent versions first, then
/// the current version it inherits through the chain.
pub(crate) fn find_version(c: &Connection, chain: &[String], key: &str, version_id: &str) -> Result<Option<(Row, Place)>> {
    let b = &chain[0];
    let own = rows::own_row(c, b, key)?;
    if let Some(r) = &own
        && !r.is_tombstone()
        && r.is_version(version_id)
    {
        return Ok(Some((r.clone(), Place::Current(0))));
    }
    if let Some(v) = own_version(c, b, key, version_id)? {
        return Ok(Some((v, Place::Noncurrent)));
    }
    if own.is_none()
        && let Some((r, i)) = rows::resolve_current(c, &chain[1..], key)?
        && r.is_version(version_id)
    {
        return Ok(Some((r, Place::Current(i + 1))));
    }
    Ok(None)
}

/// The version S3 means by `version_id` (`None`: the current one). Delete markers are errors
/// (`specific` if asked for by id), as are missing versions.
pub(crate) fn lookup(c: &Connection, chain: &[String], key: &str, version_id: Option<&str>) -> Result<(Row, Place)> {
    let found = match version_id {
        None => rows::resolve_current(c, chain, key)?.map(|(r, i)| (r, Place::Current(i))),
        Some(v) => find_version(c, chain, key, v)?,
    };
    match found {
        Some((r, _)) if r.delete_marker => Err(StoreError::DeleteMarker { key: key.to_string(), version_id: r.version_id(), modified_at: r.modified_at, specific: version_id.is_some() }),
        Some(x) => Ok(x),
        None => Err(match version_id {
            Some(v) => StoreError::NoSuchVersion(key.to_string(), v.to_string()),
            None => StoreError::NoSuchKey(key.to_string()),
        }),
    }
}

/// Whether a version may be deleted for good now. A legal hold and an unexpired COMPLIANCE
/// retention always refuse; an unexpired GOVERNANCE retention refuses unless `bypass`.
pub(crate) fn check_deletable(r: &Row, now: i64, bypass: bool) -> Result<()> {
    if r.lock.legal_hold == Some(true) {
        return Err(StoreError::ObjectLocked(format!("{} (version {}) has a legal hold", r.key, r.version_id())));
    }
    if r.lock.retained(now) && !(bypass && r.lock.mode.as_deref() == Some("GOVERNANCE")) {
        return Err(StoreError::ObjectLocked(format!("{} (version {}) is under {} retention", r.key, r.version_id(), r.lock.mode.as_deref().unwrap_or_default())));
    }
    Ok(())
}

/// A version leaves the bucket for good (its row already removed, if `owned`): into the trash
/// if the bucket keeps one (a version it only inherited gets a reference of its own there),
/// else its reference goes. Delete markers have no data and just go.
pub(crate) fn retire(c: &Connection, b: &BucketRow, r: &Row, owned: bool, now: i64) -> Result<()> {
    let Some(blob) = &r.blob_id else { return Ok(()) };
    if b.recover_days > 0 {
        if !owned {
            rows::add_ref(c, blob, 1, now)?;
        }
        rows::trash_insert(c, &b.id, r, now)?;
    } else if owned {
        rows::add_ref(c, blob, -1, now)?;
    }
    Ok(())
}

/// The default retention of a new version in a bucket with Object Lock, unless it has its own.
/// A lock on a bucket without Object Lock is refused, as on S3.
pub(crate) fn apply_lock(b: &BucketRow, lock: &mut ObjectLock, now: i64) -> Result<()> {
    let Some(cfg) = &b.object_lock else {
        if !lock.is_empty() {
            return Err(StoreError::InvalidRequest("Bucket is missing Object Lock Configuration".into()));
        }
        return Ok(());
    };
    if lock.mode.is_some() != lock.retain_until.is_some() {
        return Err(StoreError::InvalidArgument("x-amz-object-lock-retain-until-date and x-amz-object-lock-mode must both be supplied".into()));
    }
    if lock.mode.is_none()
        && let Some(mode) = &cfg.mode
    {
        let days = cfg.days.map(i64::from).or(cfg.years.map(|y| i64::from(y) * 365)).unwrap_or(0);
        lock.mode = Some(mode.clone());
        lock.retain_until = Some(now + days * DAY_MS);
    }
    Ok(())
}

/// In a versioned bucket, make `new` (a live version or a delete marker, with its vid) the
/// key's current version. The old current one becomes noncurrent, except that a new null
/// version replaces the old null version (a permanent delete, to the trash).
fn put_versioned(c: &Connection, b: &BucketRow, chain: &[String], key: &str, new: &Row, now: i64) -> Result<()> {
    let old = rows::resolve_current(c, chain, key)?;
    let null_noncurrent = if new.is_null { own_version(c, &b.id, key, "null")? } else { None };
    // Lock checks first, before anything changes.
    if new.is_null {
        if let Some((r, 0)) = &old
            && r.is_null
        {
            check_deletable(r, now, false)?;
        }
        if let Some(v) = &null_noncurrent {
            check_deletable(v, now, false)?;
        }
    }
    rows::push_down(c, &b.id, chain, key, now)?;
    if let Some((r, owner)) = &old {
        let owned = *owner == 0;
        if owned {
            rows::delete_own_row(c, &b.id, key)?;
        }
        if new.is_null && r.is_null {
            retire(c, b, r, owned, now)?;
        } else {
            if !owned && let Some(blob) = &r.blob_id {
                rows::add_ref(c, blob, 1, now)?;
            }
            rows::version_insert(c, &b.id, r, now)?;
        }
    }
    // What's left of the bucket's own row is a tombstone.
    if let Some(own) = rows::delete_own_row(c, &b.id, key)?
        && !own.is_tombstone()
    {
        return Err(StoreError::internal(format!("unexpected live row for {key}")));
    }
    if let Some(v) = &null_noncurrent {
        rows::version_delete(c, &b.id, v)?;
        retire(c, b, v, true, now)?;
    }
    rows::insert_row(c, &b.id, new)
}

/// Write `row` (whose blob reference is the caller's) as the key's new version: it gets its
/// version id and default retention here. Returns it as stored.
pub(crate) fn put_key(c: &Connection, b: &BucketRow, chain: &[String], key: &str, mut row: Row, now: i64) -> Result<Row> {
    apply_lock(b, &mut row.lock, now)?;
    row.vid = new_vid(c, &b.id, chain, key, now)?;
    row.delete_marker = false;
    match Mode::of(b) {
        Mode::Off => {
            row.is_null = true;
            rows::write_key(c, &b.id, chain, b.recover_days, key, Some(&row), true, now)?;
        }
        mode => {
            row.is_null = mode == Mode::Suspended;
            put_versioned(c, b, chain, key, &row, now)?;
        }
    }
    Ok(row)
}

/// S3's DELETE without a version id: to the trash in an unversioned bucket, a delete marker in a
/// versioned one (even if the key doesn't exist, as on S3).
pub(crate) fn delete_key(c: &Connection, b: &BucketRow, chain: &[String], key: &str, now: i64) -> Result<DeleteOutcome> {
    match Mode::of(b) {
        Mode::Off => {
            let old = rows::write_key(c, &b.id, chain, b.recover_days, key, None, true, now)?;
            Ok(DeleteOutcome { existed: old.is_some(), version_id: None, delete_marker: false })
        }
        mode => {
            let existed = rows::resolve(c, chain, key)?.is_some();
            let marker = Row::marker(key, new_vid(c, &b.id, chain, key, now)?, mode == Mode::Suspended, now);
            put_versioned(c, b, chain, key, &marker, now)?;
            Ok(DeleteOutcome { existed, version_id: Some(marker.version_id()), delete_marker: true })
        }
    }
}

/// Delete one version for good (`DELETE ?versionId=`): it goes to the trash. If it was the
/// current one, the newest noncurrent version becomes current; deleting a delete marker brings
/// back what it hid. A missing version is not an error (S3 answers 204).
pub(crate) fn delete_version(c: &Connection, b: &BucketRow, chain: &[String], key: &str, version_id: &str, bypass: bool, now: i64) -> Result<DeleteOutcome> {
    let Some((r, place)) = find_version(c, chain, key, version_id)? else {
        return Ok(DeleteOutcome { existed: false, version_id: Some(version_id.to_string()), delete_marker: false });
    };
    check_deletable(&r, now, bypass)?;
    match place {
        Place::Noncurrent => {
            rows::version_delete(c, &b.id, &r)?;
            retire(c, b, &r, true, now)?;
        }
        Place::Current(owner) => {
            rows::push_down(c, &b.id, chain, key, now)?;
            let owned = owner == 0;
            if owned {
                rows::delete_own_row(c, &b.id, key)?;
            }
            retire(c, b, &r, owned, now)?;
            promote_or_hide(c, b, chain, key, now)?;
        }
    }
    Ok(DeleteOutcome { existed: true, version_id: Some(version_id.to_string()), delete_marker: r.delete_marker })
}

/// The bucket's current row of `key` is gone: its newest noncurrent version takes over, or, with
/// none, the key is absent here (a tombstone hides what the parent has).
fn promote_or_hide(c: &Connection, b: &BucketRow, chain: &[String], key: &str, now: i64) -> Result<()> {
    if let Some((next, _)) = rows::own_versions(c, &b.id, key)?.into_iter().next() {
        rows::version_delete(c, &b.id, &next)?;
        rows::insert_row(c, &b.id, &next)?;
    } else if chain.len() > 1 && rows::resolve_current(c, &chain[1..], key)?.is_some() {
        rows::insert_row(c, &b.id, &Row::tombstone(key, now))?;
    }
    Ok(())
}

/// Bring `row` back from the trash (its trash row and reference already moved to it).
///
/// - Unversioned bucket: it becomes the current version again (the current one, if any, goes
///   to the trash), as a new write.
/// - Versioned bucket: it goes back to its place in the key's history, by version id. If the
///   key has a newer current version (or delete marker), it's restored as noncurrent; if it's
///   newer than everything there, it's current again. A restored null version replaces the
///   key's null version, as a suspended-bucket write would.
pub(crate) fn restore(c: &Connection, b: &BucketRow, chain: &[String], key: &str, mut row: Row, now: i64) -> Result<Row> {
    if Mode::of(b) == Mode::Off {
        row.modified_at = now;
        row.vid = new_vid(c, &b.id, chain, key, now)?;
        row.is_null = true;
        rows::write_key(c, &b.id, chain, b.recover_days, key, Some(&row), true, now)?;
        return Ok(row);
    }
    if row.vid.is_empty() {
        row.vid = ids::version_id(row.modified_at);
    }
    if row.is_null {
        delete_version(c, b, chain, key, "null", false, now)?;
    } else if find_version(c, chain, key, &row.vid)?.is_some() {
        return Err(StoreError::InvalidRequest(format!("version {} of {key} already exists", row.vid)));
    }
    match rows::resolve_current(c, chain, key)? {
        Some((cur, owner)) if cur.vid < row.vid => {
            // Noncurrent behind a newer version. The bucket must own the key's current row to
            // have noncurrent versions of it: an inherited one is copied in (the view doesn't
            // change, so children keep seeing the same).
            if owner != 0 {
                if let Some(blob) = &cur.blob_id {
                    rows::add_ref(c, blob, 1, now)?;
                }
                rows::delete_own_row(c, &b.id, key)?;
                rows::insert_row(c, &b.id, &cur)?;
            }
            rows::version_insert(c, &b.id, &row, now)?;
        }
        _ => put_versioned(c, b, chain, key, &row, now)?,
    }
    Ok(row)
}

/// Change a version's metadata (tags, retention, legal hold) without making a new version.
/// `f` may refuse. Children keep the old metadata of a current version (their view is frozen).
pub(crate) fn update_version(c: &Connection, b: &BucketRow, chain: &[String], key: &str, version_id: Option<&str>, now: i64, f: impl FnOnce(&mut Row) -> Result<()>) -> Result<Row> {
    let (old, place) = lookup(c, chain, key, version_id)?;
    let mut new = old.clone();
    f(&mut new)?;
    match place {
        Place::Noncurrent => {
            let at: i64 = c.query_row("SELECT noncurrent_at FROM versions WHERE bucket_id = ?1 AND key = ?2 AND vid = ?3", params![b.id, key, old.vid], |r| r.get(0))?;
            rows::version_delete(c, &b.id, &old)?;
            rows::version_insert(c, &b.id, &new, at)?;
        }
        Place::Current(owner) => {
            rows::push_down(c, &b.id, chain, key, now)?;
            if owner == 0 {
                rows::delete_own_row(c, &b.id, key)?;
            } else if let Some(blob) = &old.blob_id {
                rows::add_ref(c, blob, 1, now)?;
            }
            rows::insert_row(c, &b.id, &new)?;
        }
    }
    Ok(new)
}

/// S3's rules for changing a version's retention. COMPLIANCE can only be extended (never
/// shortened, removed or turned into GOVERNANCE) while it runs; GOVERNANCE can be extended or
/// made COMPLIANCE, and shortened or removed only with `bypass`. An expired one can be
/// anything.
pub(crate) fn check_retention_change(old: &ObjectLock, new: Option<(&str, i64)>, now: i64, bypass: bool) -> Result<()> {
    if !old.retained(now) {
        return Ok(());
    }
    let old_until = old.retain_until.unwrap_or(0);
    let weaker = match new {
        None => true,
        Some((mode, until)) => until < old_until || (old.mode.as_deref() == Some("COMPLIANCE") && mode != "COMPLIANCE"),
    };
    if !weaker {
        return Ok(());
    }
    if old.mode.as_deref() == Some("GOVERNANCE") && bypass {
        return Ok(());
    }
    Err(StoreError::ObjectLocked("the version's retention can't be shortened or removed".into()))
}

/// One page of S3's ListObjectVersions over a bucket's view: for each key in order, its current
/// version (inherited through the chain or its own), then its own noncurrent versions, newest
/// first. Keys roll up into common prefixes.
pub(crate) fn list_versions(c: &Connection, chain: &[String], q: &VersionsQuery) -> Result<VersionsPage> {
    let delimiter = q.delimiter.as_deref().filter(|d| !d.is_empty());
    let prefix = q.prefix.as_str();
    // Where to start: after the key marker (or the whole common prefix it names), or inside it
    // after a version.
    let mut skip_in_first: Option<(String, String)> = None; // (key, vid after which to resume)
    let (from, inclusive) = match q.key_marker.as_deref().filter(|k| !k.is_empty()) {
        None => (prefix.to_string(), true),
        Some(km) => match q.version_id_marker.as_deref().filter(|v| !v.is_empty()) {
            Some(vm) => {
                let after = if vm == "null" { find_version(c, chain, km, "null")?.map(|(r, _)| r.vid) } else { Some(vm.to_string()) };
                match after {
                    Some(vid) => {
                        skip_in_first = Some((km.to_string(), vid));
                        (km.to_string(), true)
                    }
                    None => (km.to_string(), false),
                }
            }
            None => match rows::common_prefix(km, prefix, delimiter) {
                Some(cp) if cp == km => (rows::past_prefix(km), true),
                _ => (km.to_string(), false),
            },
        },
    };
    let (from, inclusive) = if from.as_str() < prefix { (prefix.to_string(), true) } else { (from, inclusive) };
    let mut current = Peek::new(rows::Merger::current(c, chain, prefix, &from, inclusive));
    let mut noncurrent = Noncurrent::new(&chain[0], prefix, &from, inclusive);
    let mut page = VersionsPage::default();
    let mut n = 0usize;
    'keys: loop {
        let ck = current.peek(c)?.map(|r| r.key.clone());
        let nk = noncurrent.peek(c)?.map(|(r, _)| r.key.clone());
        let key = match (ck, nk) {
            (None, None) => break,
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (Some(a), Some(b)) => a.min(b),
        };
        if let Some(cp) = rows::common_prefix(&key, prefix, delimiter) {
            let cp = cp.to_string();
            if n == q.max_keys {
                page.truncated = true;
                break;
            }
            let past = rows::past_prefix(&cp);
            current.seek(&past);
            noncurrent.seek(&past);
            page.next_key_marker = Some(cp.clone());
            page.next_version_id_marker = None;
            page.prefixes.push(cp);
            n += 1;
            continue;
        }
        let mut entries: Vec<(Row, bool)> = Vec::new();
        if current.peek(c)?.is_some_and(|r| r.key == key) {
            entries.push((current.take().expect("peeked"), true));
        }
        while noncurrent.peek(c)?.is_some_and(|(r, _)| r.key == key) {
            entries.push((noncurrent.take().expect("peeked").0, false));
        }
        for (r, latest) in entries {
            if let Some((k, after)) = &skip_in_first
                && *k == key
                && r.vid <= *after
            {
                continue;
            }
            if n == q.max_keys {
                page.truncated = true;
                break 'keys;
            }
            page.next_key_marker = Some(key.clone());
            page.next_version_id_marker = Some(r.version_id());
            page.versions.push(ObjectVersion {
                key: r.key.clone(),
                version_id: r.version_id(),
                is_latest: latest,
                delete_marker: r.delete_marker,
                size: r.size,
                etag: r.etag.clone(),
                last_modified: r.modified_at,
                checksum: r.checksum.clone(),
            });
            n += 1;
        }
    }
    if !page.truncated {
        page.next_key_marker = None;
        page.next_version_id_marker = None;
    }
    Ok(page)
}

/// A [`rows::Merger`] with one row of lookahead.
struct Peek<'a> {
    m: rows::Merger<'a>,
    next: Option<Row>,
    done: bool,
}

impl<'a> Peek<'a> {
    fn new(m: rows::Merger<'a>) -> Self {
        Self { m, next: None, done: false }
    }

    fn peek(&mut self, _c: &Connection) -> Result<Option<&Row>> {
        if self.next.is_none() && !self.done {
            match self.m.next_entry()? {
                Some(rows::Entry::Object(r)) => self.next = Some(*r),
                Some(rows::Entry::Prefix(_)) => return Err(StoreError::internal("unexpected prefix from an unrolled listing")),
                None => self.done = true,
            }
        }
        Ok(self.next.as_ref())
    }

    fn take(&mut self) -> Option<Row> {
        self.next.take()
    }

    fn seek(&mut self, from: &str) {
        self.next = None;
        self.done = false;
        self.m.seek(from);
    }
}

/// The bucket's noncurrent versions in (key, newest first) order, a page at a time.
struct Noncurrent {
    bucket: String,
    buf: std::collections::VecDeque<(Row, i64)>,
    /// Resume after this (key, vid): vid `""` takes all of the key, `"~"` (after every vid)
    /// none of it.
    key: String,
    vid: String,
    done: bool,
    prefix: String,
}

impl Noncurrent {
    const PAGE: i64 = 256;

    fn new(bucket: &str, prefix: &str, from: &str, inclusive: bool) -> Self {
        let vid = if inclusive { "" } else { "~" };
        Self { bucket: bucket.to_string(), buf: Default::default(), key: from.to_string(), vid: vid.to_string(), done: false, prefix: prefix.to_string() }
    }

    fn peek(&mut self, c: &Connection) -> Result<Option<&(Row, i64)>> {
        if self.buf.is_empty() && !self.done {
            let mut st = c.prepare_cached(&format!(
                "SELECT {}, noncurrent_at FROM versions WHERE bucket_id = ?1 AND (key > ?2 OR (key = ?2 AND vid > ?3)) ORDER BY key, vid LIMIT ?4",
                rows::COLS
            ))?;
            let got = st.query_map(params![self.bucket, self.key, self.vid, Self::PAGE], |r| Ok((rows::row_at(r, 0)?, r.get::<_, i64>(rows::COLS_LEN)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if (got.len() as i64) < Self::PAGE {
                self.done = true;
            }
            if let Some((r, _)) = got.last() {
                self.key = r.key.clone();
                self.vid = r.vid.clone();
            }
            for v in got {
                if !v.0.key.starts_with(&self.prefix) {
                    self.done = true;
                    break;
                }
                self.buf.push_back(v);
            }
        }
        Ok(self.buf.front())
    }

    fn take(&mut self) -> Option<(Row, i64)> {
        self.buf.pop_front()
    }

    fn seek(&mut self, from: &str) {
        self.buf.clear();
        self.key = from.to_string();
        self.vid = String::new();
        self.done = false;
    }
}
