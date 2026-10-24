//! Lifecycle evaluation (the daily job): expiration of current versions (by age or date),
//! noncurrent version expiration, and removal of expired delete markers, filtered by prefix,
//! tags and size. Transitions are stored but never acted on: Intelligent-Tiering already moves
//! data by access, and no other storage class is ever reported.
//!
//! Everything a rule would delete is listed first and checked again inside the transaction
//! that deletes it (still that version, still old enough, still matching, not locked), so a
//! write that lands meanwhile is never expired by an earlier listing. Expiration is a normal
//! delete (to the trash, or a delete marker in a versioned bucket); noncurrent expiration is a
//! permanent delete, to the trash.

use crate::error::Result;
use crate::model::*;
use crate::rows;
use crate::store::{Store, find_bucket};
use crate::versioning::{self, DAY_MS};
use rusqlite::params;

const BATCH: i64 = 1000;

impl Store {
    /// Apply every bucket's enabled lifecycle rules once. Returns how many versions (and delete
    /// markers) were expired.
    pub async fn run_lifecycle(&self) -> Result<u64> {
        let now = self.now();
        let mut n = 0;
        for b in self.buckets()? {
            for rule in b.lifecycle.iter().filter(|r| r.enabled) {
                n += self.expire_current_versions(&b.name, rule, now).await?;
                if let Some(days) = rule.noncurrent_days {
                    n += self.expire_noncurrent(&b.name, rule, now - days as i64 * DAY_MS, now).await?;
                }
                if rule.expire_delete_markers {
                    n += self.expire_delete_markers(&b.name, rule).await?;
                }
            }
        }
        Ok(n)
    }

    /// `Expiration` `Days` or `Date`: current versions written before the cutoff.
    async fn expire_current_versions(&self, bucket: &str, rule: &LifecycleRule, now: i64) -> Result<u64> {
        let cutoff = match (rule.expire_days, rule.expire_at) {
            (Some(days), _) => now - days as i64 * DAY_MS,
            // On and after the date, everything matching that existed when the run started.
            (None, Some(at)) if at <= now => now,
            _ => return Ok(0),
        };
        let mut n = 0;
        let mut after: Option<String> = None;
        loop {
            let page = self.list_objects(bucket, &ListQuery { prefix: rule.prefix.clone(), start_after: after.clone(), max_keys: BATCH as usize, ..Default::default() })?;
            let expired: Vec<(String, String)> = page.objects.iter().filter(|o| o.modified_at < cutoff && rule.matches(&o.key, &o.headers.tags, o.size)).map(|o| (o.key.clone(), o.etag.clone())).collect();
            if !expired.is_empty() {
                n += self.expire_current(bucket, &expired, cutoff, Some(rule)).await?;
            }
            if !page.truncated {
                return Ok(n);
            }
            after = page.next;
        }
    }

    /// `NoncurrentVersionExpiration`: versions noncurrent since before `cutoff`, beyond the
    /// newest `NewerNoncurrentVersions`, are deleted for good (to the trash). Locked ones stay.
    async fn expire_noncurrent(&self, bucket: &str, rule: &LifecycleRule, cutoff: i64, now: i64) -> Result<u64> {
        let keep = rule.newer_noncurrent_versions.unwrap_or(0) as usize;
        let mut n = 0;
        let mut after = String::new();
        loop {
            let keys: Vec<String> = self.read(|c| {
                let b = find_bucket(c, bucket)?;
                Ok(c.prepare_cached("SELECT DISTINCT key FROM versions WHERE bucket_id = ?1 AND key >= ?2 AND key > ?3 AND noncurrent_at < ?4 ORDER BY key LIMIT ?5")?
                    .query_map(params![b.id, rule.prefix, after, cutoff, BATCH], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?)
            })?;
            let Some(last) = keys.last().cloned() else { return Ok(n) };
            let keys: Vec<String> = keys.into_iter().filter(|k| k.starts_with(&rule.prefix)).collect();
            let done = keys.is_empty();
            n += self
                .write(|c| {
                    let b = find_bucket(c, bucket)?;
                    let mut n = 0;
                    for key in &keys {
                        for (i, (v, at)) in rows::own_versions(c, &b.id, key)?.into_iter().enumerate() {
                            if i < keep || at >= cutoff || !rule.matches(key, &v.headers.tags, v.size) || v.lock.locked(now) {
                                continue;
                            }
                            rows::version_delete(c, &b.id, &v)?;
                            versioning::retire(c, &b, &v, true, now)?;
                            let o = DeleteOutcome { existed: true, version_id: Some(v.version_id()), delete_marker: v.delete_marker };
                            crate::events::emit_delete(c, &b, key, &o, true, "LifecycleExpiration", now)?;
                            n += 1;
                        }
                    }
                    Ok(n)
                })
                .await?;
            if done {
                return Ok(n);
            }
            after = last;
        }
    }

    /// `ExpiredObjectDeleteMarker`: delete markers that are a key's only version left.
    async fn expire_delete_markers(&self, bucket: &str, rule: &LifecycleRule) -> Result<u64> {
        let mut n = 0;
        let mut after = String::new();
        loop {
            let keys: Vec<String> = self.read(|c| {
                let b = find_bucket(c, bucket)?;
                Ok(c.prepare_cached(
                    "SELECT key FROM objects o WHERE bucket_id = ?1 AND key >= ?2 AND key > ?3 AND delete_marker = 1
                       AND NOT EXISTS (SELECT 1 FROM versions v WHERE v.bucket_id = o.bucket_id AND v.key = o.key) ORDER BY key LIMIT ?4",
                )?
                .query_map(params![b.id, rule.prefix, after, BATCH], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?)
            })?;
            let Some(last) = keys.last().cloned() else { return Ok(n) };
            let keys: Vec<String> = keys.into_iter().filter(|k| k.starts_with(&rule.prefix)).collect();
            let done = keys.is_empty();
            let now = self.now();
            n += self
                .write(|c| {
                    let b = find_bucket(c, bucket)?;
                    let chain = rows::chain(c, &b.id)?;
                    let mut n = 0;
                    for key in &keys {
                        let Some(r) = rows::own_row(c, &b.id, key)? else { continue };
                        if !r.delete_marker || !rows::own_versions(c, &b.id, key)?.is_empty() {
                            continue;
                        }
                        let o = versioning::delete_version(c, &b, &chain, key, &r.version_id(), false, now)?;
                        crate::events::emit_delete(c, &b, key, &o, true, "LifecycleExpiration", now)?;
                        n += 1;
                    }
                    Ok(n)
                })
                .await?;
            if done {
                return Ok(n);
            }
            after = last;
        }
    }
}
