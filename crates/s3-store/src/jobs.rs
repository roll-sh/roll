//! Background jobs: the sweeper (trash expiry, quarantined blob deletion, stale uploads),
//! lifecycle expiry, the orphan scan and the scrub. Each can also be run by hand.

use crate::error::Result;
use crate::ids;
use crate::model::*;
use crate::rows;
use crate::store::{Store, every, find_bucket_id};
use rusqlite::{OptionalExtension, params};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const DAY_MS: i64 = 86_400_000;

impl Store {
    pub(crate) fn start_jobs(self: &Arc<Self>) {
        every(self, self.cfg.meter_interval, "meter", |s| async move { s.flush_meter() });
        every(self, self.cfg.sweep_interval, "sweep", |s| async move {
            let st = s.sweep().await?;
            if st.trash_expired + st.blobs_deleted + st.uploads_aborted > 0 {
                tracing::info!(trash_expired = st.trash_expired, blobs_deleted = st.blobs_deleted, bytes = st.blob_bytes_deleted, uploads_aborted = st.uploads_aborted, "storage sweep");
            }
            Ok(())
        });
        every(self, self.cfg.lifecycle_interval, "lifecycle", |s| async move {
            let n = s.run_lifecycle().await?;
            if n > 0 {
                tracing::info!(expired = n, "storage lifecycle");
            }
            Ok(())
        });
        every(self, self.cfg.orphan_interval, "orphan scan", |s| async move {
            let st = s.orphan_scan().await?;
            if st.deleted > 0 {
                tracing::info!(deleted = st.deleted, bytes = st.deleted_bytes, scanned = st.scanned, "storage orphan scan");
            }
            Ok(())
        });
        every(self, self.cfg.scrub_interval, "scrub", |s| async move { s.scrub().await.map(|_| ()) });
    }

    /// Expire trash past each bucket's recovery window, delete blobs that have been
    /// unreferenced longer than the quarantine, and abort stale multipart uploads.
    pub async fn sweep(&self) -> Result<SweepStats> {
        let mut st = SweepStats::default();
        let now = self.now();
        self.barrier("sweep").await?;

        // Trash rows past their bucket's window. A version under Object Lock (one whose
        // GOVERNANCE retention was bypassed to delete it) stays until its lock ends.
        loop {
            let n = self
                .write(|c| {
                    let expired = c
                        .prepare(
                            "SELECT t.bucket_id, t.key, t.deleted_at, t.blob_id, t.size FROM trash t JOIN buckets b ON b.id = t.bucket_id
                             WHERE t.deleted_at < ?1 - b.recover_days * 86400000
                               AND coalesce(t.legal_hold, 0) = 0 AND NOT (t.lock_mode IS NOT NULL AND coalesce(t.lock_until, 0) > ?1) LIMIT 1000",
                        )?
                        .query_map([now], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    for (bucket, key, at, blob, size) in &expired {
                        rows::trash_delete(c, bucket, key, *at, *size as u64)?;
                        rows::add_ref(c, blob, -1, now)?;
                    }
                    Ok(expired.len() as u64)
                })
                .await?;
            st.trash_expired += n;
            if n < 1000 {
                break;
            }
        }

        // Blobs unreferenced for longer than the quarantine: object storage first, then the row
        // (a crash in between leaves a row that the next sweep deletes again).
        let cutoff = now - self.cfg.quarantine.as_millis() as i64;
        loop {
            let due: Vec<(String, i64)> = self.read(|c| {
                Ok(c.prepare("SELECT id, size FROM blobs WHERE refs = 0 AND unreferenced_at < ?1 LIMIT 1000")?
                    .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })?;
            if due.is_empty() {
                break;
            }
            let mut deleted = Vec::with_capacity(due.len());
            for (id, size) in &due {
                // A row still pointing at it means `refs` is wrong: never delete it.
                if self.blob_referenced(id)? {
                    tracing::error!(blob = id, "blob has refs = 0 but a current, noncurrent or trashed version references it; kept");
                    continue;
                }
                match self.backend.delete(&ids::blob_key(id)).await {
                    Ok(()) => deleted.push((id.clone(), *size)),
                    Err(e) => tracing::warn!(error = %e, blob = id, "deleting an unreferenced blob failed; retrying next sweep"),
                }
            }
            let gone = self
                .write(|c| {
                    let mut n = 0;
                    for (id, _) in &deleted {
                        n += c.execute("DELETE FROM blobs WHERE id = ?1 AND refs = 0", [id])?;
                        crate::sse::forget_if_unused(c, id)?;
                        crate::torrent::forget(c, id)?;
                    }
                    Ok(n)
                })
                .await?;
            st.blobs_deleted += gone as u64;
            st.blob_bytes_deleted += deleted.iter().map(|(_, s)| *s as u64).sum::<u64>();
            if deleted.len() < due.len() || due.len() < 1000 {
                break;
            }
        }

        // Multipart uploads older than their bucket's abort rule (or the default).
        let uploads: Vec<(String, String, String, i64)> = self.read(|c| {
            Ok(c.prepare("SELECT id, bucket_id, key, created_at FROM uploads ORDER BY created_at")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for (id, bucket_id, key, created_at) in uploads {
            let rules = self.read(|c| Ok(find_bucket_id(c, &bucket_id)?.map(|b| b.lifecycle).unwrap_or_default()))?;
            let days = rules
                .iter()
                .filter(|r| r.enabled && key.starts_with(&r.prefix))
                .filter_map(|r| r.abort_multipart_days)
                .min()
                .unwrap_or(self.cfg.upload_expiry_days)
                .min(self.cfg.upload_expiry_days);
            if created_at < now - days as i64 * DAY_MS {
                match self.abort_upload(&id).await {
                    Ok(()) => st.uploads_aborted += 1,
                    Err(crate::StoreError::NoSuchUpload(_)) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(st)
    }

    /// Delete blobs in object storage that no metadata knows about: left by a crash between a
    /// blob's upload and its commit (never acknowledged). Two phases, a day apart:
    ///
    /// 1. A scan records each unknown blob as a candidate, in a durable write. A node that lost
    ///    the log to another one (and so doesn't know that node's new blobs) can't record any.
    /// 2. A later scan deletes candidates recorded at least `orphan_min_age` ago that are still
    ///    unknown.
    ///
    /// Blobs older than `orphan_max_age` without metadata mean metadata went missing: they're
    /// kept and reported instead.
    pub async fn orphan_scan(&self) -> Result<OrphanStats> {
        let mut st = OrphanStats::default();
        let now = self.now();
        let min_age = self.cfg.orphan_min_age.as_millis() as i64;
        let max_age = self.cfg.orphan_max_age.as_millis() as i64;
        self.barrier("orphan_scan").await?;
        let recorded: std::collections::HashMap<String, i64> = self.read(|c| {
            Ok(c.prepare("SELECT blob_key, found_at FROM orphans")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
        })?;
        let mut drop_rows: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut found: Vec<(String, u64)> = Vec::new();
        let mut unexpected = 0u64;
        let mut after: Option<String> = None;
        loop {
            let (page, more) = self.backend.list_page("s3/blobs/", after.as_deref(), 1000).await?;
            after = page.last().map(|l| l.key.clone()).or(after);
            for l in &page {
                st.scanned += 1;
                let Some(id) = ids::blob_of_key(&l.key) else { continue };
                seen.insert(l.key.clone());
                // In flight first, then the metadata: a blob is registered before its upload
                // starts and released only after its commit.
                if self.inflight.lock().unwrap().contains(id) || self.blob_known(id)? {
                    if recorded.contains_key(&l.key) {
                        drop_rows.push(l.key.clone());
                    }
                    continue;
                }
                match recorded.get(&l.key) {
                    Some(at) if *at <= now - min_age => {
                        self.backend.delete(&l.key).await?;
                        drop_rows.push(l.key.clone());
                        st.deleted += 1;
                        st.deleted_bytes += l.size;
                    }
                    Some(_) => {}
                    None if now - l.modified_ms > max_age => {
                        unexpected += 1;
                        tracing::error!(blob = id, key = l.key, age_days = (now - l.modified_ms) / DAY_MS, "blob in object storage without metadata (kept)");
                    }
                    None => found.push((l.key.clone(), l.size)),
                }
            }
            if !more {
                break;
            }
        }
        drop_rows.extend(recorded.keys().filter(|k| !seen.contains(*k)).cloned());
        if !drop_rows.is_empty() || !found.is_empty() {
            self.write(|c| {
                for k in &drop_rows {
                    c.execute("DELETE FROM orphans WHERE blob_key = ?1", [k])?;
                }
                for (k, size) in &found {
                    c.execute("INSERT OR IGNORE INTO orphans (blob_key, size, found_at) VALUES (?1, ?2, ?3)", params![k, *size as i64, now])?;
                }
                Ok(())
            })
            .await?;
        }
        st.candidates = found.len() as u64;
        self.unexpected_orphans.store(unexpected, Ordering::Relaxed);
        Ok(st)
    }

    /// A durable write that always reaches the log: it fails on a node that isn't the log's
    /// writer any more, so jobs that delete things start with one.
    pub(crate) async fn barrier(&self, job: &str) -> Result<()> {
        let nonce = ids::blob_id();
        self.write(|c| {
            c.execute("INSERT INTO state (k, v) VALUES (?1, ?2) ON CONFLICT (k) DO UPDATE SET v = excluded.v", params![format!("job:{job}"), nonce])?;
            Ok(())
        })
        .await
    }

    /// Whether the metadata knows the blob at all: a `blobs` row, an upload, or any version
    /// (current, noncurrent or trashed) pointing at it.
    fn blob_known(&self, id: &str) -> Result<bool> {
        Ok(self.read(|c| Ok(c.query_row("SELECT 1 FROM blobs WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some()))? || self.blob_referenced(id)?)
    }

    /// Whether any version row (current, noncurrent, trashed) or upload points at the blob.
    pub(crate) fn blob_referenced(&self, id: &str) -> Result<bool> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT 1 FROM objects WHERE blob_id = ?1 UNION ALL SELECT 1 FROM versions WHERE blob_id = ?1
                 UNION ALL SELECT 1 FROM trash WHERE blob_id = ?1 UNION ALL SELECT 1 FROM uploads WHERE blob_id = ?1 LIMIT 1",
            )?
            .exists([id])?)
        })
    }

    /// Check every blob the metadata knows (created before the scrub started) is in object
    /// storage with the right size. Never deletes anything; problems are logged and counted.
    pub async fn scrub(&self) -> Result<ScrubReport> {
        let started = self.now();
        let mut report = ScrubReport { last_at: Some(started), ..Default::default() };
        let mut listing: std::collections::VecDeque<crate::backend::Listed> = Default::default();
        let mut list_after: Option<String> = None;
        let mut list_more = true;
        let mut meta_after = String::new();
        loop {
            let batch: Vec<(String, i64)> = self.read(|c| {
                Ok(c.prepare("SELECT id, size FROM blobs WHERE id > ?1 AND created_at < ?2 ORDER BY id LIMIT 1000")?
                    .query_map(params![meta_after, started - 60_000], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })?;
            let Some(last) = batch.last() else { break };
            meta_after = last.0.clone();
            for (id, size) in batch {
                let key = ids::blob_key(&id);
                // Advance the listing to this key.
                loop {
                    if let Some(first) = listing.front()
                        && first.key < key
                    {
                        listing.pop_front();
                        continue;
                    }
                    if listing.is_empty() && list_more {
                        let (page, more) = self.backend.list_page("s3/blobs/", list_after.as_deref(), 1000).await?;
                        list_after = page.last().map(|l| l.key.clone()).or(list_after);
                        list_more = more;
                        listing = page.into();
                        continue;
                    }
                    break;
                }
                report.checked += 1;
                match listing.front() {
                    Some(l) if l.key == key => {
                        if l.size != size as u64 {
                            report.mismatched += 1;
                            tracing::error!(blob = id, expected = size, found = l.size, "blob has the wrong size in object storage");
                            if report.examples.len() < 20 {
                                report.examples.push(id);
                            }
                        }
                    }
                    _ => {
                        // Deleted by the sweeper since the listing? Then its row is gone too.
                        let still = self.read(|c| Ok(c.query_row("SELECT 1 FROM blobs WHERE id = ?1", [&id], |_| Ok(())).optional()?.is_some()))?;
                        if still && self.backend.head(&key).await?.is_none() {
                            report.missing += 1;
                            tracing::error!(blob = id, "blob missing from object storage");
                            if report.examples.len() < 20 {
                                report.examples.push(id);
                            }
                        }
                    }
                }
            }
        }
        // Versions (current, noncurrent or trashed) whose blob has no metadata at all: their
        // bytes could be deleted as orphans, so they count as missing.
        let dangling: Vec<String> = self.read(|c| {
            Ok(c.prepare(
                "SELECT DISTINCT blob_id FROM (SELECT blob_id FROM objects WHERE blob_id IS NOT NULL UNION ALL SELECT blob_id FROM versions WHERE blob_id IS NOT NULL
                   UNION ALL SELECT blob_id FROM trash) WHERE blob_id NOT IN (SELECT id FROM blobs) LIMIT 1000",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?)
        })?;
        for id in dangling {
            report.missing += 1;
            tracing::error!(blob = id, "a version references a blob the metadata doesn't know");
            if report.examples.len() < 20 {
                report.examples.push(id);
            }
        }
        *self.scrub.lock().unwrap() = report.clone();
        let v = serde_json::to_string(&report)?;
        self.write_relaxed(|c| {
            c.execute("INSERT INTO state (k, v) VALUES ('scrub', ?1) ON CONFLICT (k) DO UPDATE SET v = excluded.v", [&v])?;
            Ok(())
        })?;
        if report.missing + report.mismatched > 0 {
            tracing::error!(missing = report.missing, mismatched = report.mismatched, checked = report.checked, "storage scrub found problems");
        } else {
            tracing::info!(checked = report.checked, "storage scrub clean");
        }
        Ok(report)
    }
}
