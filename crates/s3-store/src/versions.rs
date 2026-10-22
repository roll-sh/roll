//! The store's versioning and Object Lock API: versions by id, permanent deletes, version
//! listings, bucket versioning and lock configuration, retention and legal holds. The rules are
//! in `versioning`.

use crate::error::{Result, StoreError};
use crate::events;
use crate::model::*;
use crate::rows;
use crate::store::{Store, find_bucket};
use crate::validate::{self, bare_etag};
use crate::versioning::{self, Mode};
use std::collections::BTreeMap;

fn etag_matches(cond: &str, etag: &str) -> bool {
    cond.split(',').map(bare_etag).any(|c| c == "*" || c == etag)
}

impl Store {
    /// A version of `key` (`None`: the current one). A delete marker is
    /// [`StoreError::DeleteMarker`]; a missing version [`StoreError::NoSuchVersion`].
    pub fn get_object_version(&self, bucket: &str, key: &str, version_id: Option<&str>) -> Result<ObjectMeta> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            versioning::lookup(c, &chain, key, version_id)?.0.meta()
        })
    }

    /// S3's DeleteObject. Without a version id: to the trash (unversioned), or a new delete
    /// marker (versioned). With one: that version is deleted for good (to the trash), unless
    /// Object Lock protects it (`bypass` lifts a GOVERNANCE retention). `if_match`: only if the
    /// version has that ETag.
    pub async fn delete_object_version(&self, bucket: &str, key: &str, version_id: Option<&str>, if_match: Option<&str>, bypass: bool) -> Result<DeleteOutcome> {
        let now = self.now();
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            if let Some(m) = if_match {
                let target = match version_id {
                    None => rows::resolve(c, &chain, key)?.map(|(r, _)| r),
                    Some(v) => versioning::find_version(c, &chain, key, v)?.map(|(r, _)| r),
                };
                match target {
                    None => return Err(StoreError::NoSuchKey(key.to_string())),
                    Some(r) if r.delete_marker || !etag_matches(m, &r.etag) => return Err(StoreError::PreconditionFailed),
                    Some(_) => {}
                }
            }
            let o = match version_id {
                None => versioning::delete_key(c, &b, &chain, key, now),
                Some(v) => versioning::delete_version(c, &b, &chain, key, v, bypass, now),
            }?;
            events::emit_delete(c, &b, key, &o, version_id.is_some(), "ObjectRemoved", now)?;
            Ok(o)
        })
        .await
    }

    /// S3's DeleteObjects: many keys (each with an optional version id) in one durable write.
    /// Each result is that key's outcome or why it was refused.
    pub async fn delete_versions(&self, bucket: &str, items: &[(String, Option<String>)], bypass: bool) -> Result<Vec<Result<DeleteOutcome>>> {
        let now = self.now();
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            let mut out = Vec::with_capacity(items.len());
            for (k, v) in items {
                // Every refusal comes before its key's changes, so the others still apply.
                let r = validate::key(k).and_then(|_| match v {
                    None => versioning::delete_key(c, &b, &chain, k, now),
                    Some(v) => versioning::delete_version(c, &b, &chain, k, v, bypass, now),
                });
                if let Ok(o) = &r {
                    events::emit_delete(c, &b, k, o, v.is_some(), "ObjectRemoved", now)?;
                }
                match r {
                    Err(StoreError::Internal(e)) => return Err(StoreError::Internal(e)),
                    r => out.push(r),
                }
            }
            Ok(out)
        })
        .await
    }

    /// One page of S3's ListObjectVersions: for each key, its current version (or delete
    /// marker), then its noncurrent versions, newest first.
    pub fn list_versions(&self, bucket: &str, q: &VersionsQuery) -> Result<VersionsPage> {
        if q.version_id_marker.as_deref().is_some_and(|v| !v.is_empty()) && q.key_marker.as_deref().is_none_or(str::is_empty) {
            return Err(StoreError::InvalidArgument("A version-id marker cannot be specified without a key marker.".into()));
        }
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            versioning::list_versions(c, &chain, q)
        })
    }

    /// The tags of a version (`None`: the current one).
    pub fn get_version_tags(&self, bucket: &str, key: &str, version_id: Option<&str>) -> Result<(String, BTreeMap<String, String>)> {
        let m = self.get_object_version(bucket, key, version_id)?;
        Ok((m.version_id, m.headers.tags))
    }

    /// Replace a version's tags (not a new version: Last-Modified stays). Returns the version.
    pub async fn put_version_tags(&self, bucket: &str, key: &str, version_id: Option<&str>, tags: BTreeMap<String, String>) -> Result<ObjectMeta> {
        validate::tags(&tags)?;
        let now = self.now();
        let name = if tags.is_empty() { "ObjectTagging:Delete" } else { "ObjectTagging:Put" };
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            let r = versioning::update_version(c, &b, &chain, key, version_id, now, |r| {
                r.headers.tags = tags;
                Ok(())
            })?;
            events::emit_update(c, &b, name, &r, now)?;
            r.meta()
        })
        .await
    }

    /// Turn on Object Lock (versioning must be enabled; it can't be turned off again) and set
    /// its default retention: `mode` with `days` or `years`, or none.
    pub async fn put_object_lock(&self, bucket: &str, cfg: ObjectLockConfig) -> Result<Bucket> {
        if let Some(m) = &cfg.mode {
            if m != "GOVERNANCE" && m != "COMPLIANCE" {
                return Err(StoreError::InvalidArgument("the retention mode is GOVERNANCE or COMPLIANCE".into()));
            }
            if cfg.days.is_some() == cfg.years.is_some() || cfg.days == Some(0) || cfg.years == Some(0) {
                return Err(StoreError::InvalidArgument("a default retention needs a positive number of days or years".into()));
            }
        } else if cfg.days.is_some() || cfg.years.is_some() {
            return Err(StoreError::InvalidArgument("a default retention needs a mode".into()));
        }
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            if b.object_lock.is_none() && Mode::of(&b) != Mode::Enabled {
                return Err(StoreError::InvalidBucketState("Versioning must be 'Enabled' on the bucket to apply a Object Lock configuration".into()));
            }
            c.execute("UPDATE buckets SET object_lock = ?2 WHERE id = ?1", rusqlite::params![b.id, rows::json(&cfg)?])?;
            Ok(())
        })
        .await?;
        self.bucket(bucket)
    }

    /// Set (`Some((mode, retain until ms))`) or remove (`None`) a version's retention, under
    /// S3's rules: COMPLIANCE can only be extended; GOVERNANCE can be shortened or removed only
    /// with `bypass`.
    pub async fn put_retention(&self, bucket: &str, key: &str, version_id: Option<&str>, retention: Option<(String, i64)>, bypass: bool) -> Result<ObjectMeta> {
        let now = self.now();
        if let Some((mode, until)) = &retention {
            if mode != "GOVERNANCE" && mode != "COMPLIANCE" {
                return Err(StoreError::InvalidArgument("the retention mode is GOVERNANCE or COMPLIANCE".into()));
            }
            if *until <= now {
                return Err(StoreError::InvalidArgument("The retain until date must be in the future!".into()));
            }
        }
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            if b.object_lock.is_none() {
                return Err(StoreError::InvalidRequest("Bucket is missing Object Lock Configuration".into()));
            }
            let chain = rows::chain(c, &b.id)?;
            versioning::update_version(c, &b, &chain, key, version_id, now, |r| {
                versioning::check_retention_change(&r.lock, retention.as_ref().map(|(m, u)| (m.as_str(), *u)), now, bypass)?;
                match &retention {
                    Some((m, u)) => {
                        r.lock.mode = Some(m.clone());
                        r.lock.retain_until = Some(*u);
                    }
                    None => {
                        r.lock.mode = None;
                        r.lock.retain_until = None;
                    }
                }
                Ok(())
            })?
            .meta()
        })
        .await
    }

    /// Put a legal hold on a version, or lift it.
    pub async fn put_legal_hold(&self, bucket: &str, key: &str, version_id: Option<&str>, on: bool) -> Result<ObjectMeta> {
        let now = self.now();
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            if b.object_lock.is_none() {
                return Err(StoreError::InvalidRequest("Bucket is missing Object Lock Configuration".into()));
            }
            let chain = rows::chain(c, &b.id)?;
            versioning::update_version(c, &b, &chain, key, version_id, now, |r| {
                r.lock.legal_hold = Some(on);
                Ok(())
            })?
            .meta()
        })
        .await
    }
}
