//! `RenameObject` (S3 Express; on general purpose buckets a roll extension).
//!
//! Metadata only, in one transaction: the destination gets a row for the source's blob (one
//! more reference), the source's row goes (one less), so the blob is never copied and its
//! reference count never drops to zero on the way. Conditions on both sides are checked inside
//! the same transaction, so a concurrent write to either key either happens before the rename
//! (and is checked) or after it.
//!
//! - The renamed object keeps its data, ETag, checksum, metadata, tags and Last-Modified; it's
//!   a new version of the destination key.
//! - Whatever the destination held is overwritten as by any write: to the trash in an
//!   unversioned bucket, noncurrent in a versioned one.
//! - The source disappears without going to the trash (its bytes live on at the destination).
//!   In a versioned bucket it gets a delete marker instead, so its history stays, and the
//!   version at the destination is a second reference to the same blob.
//! - Branches keep their view as for any write (push-down).

use crate::error::{Result, StoreError};
use crate::events;
use crate::model::{DeleteOutcome, ObjectMeta};
use crate::objects::etag_matches;
use crate::rows::{self, Row};
use crate::store::{Store, find_bucket};
use crate::versioning::{self, Mode};

/// Conditions on one side of a rename (times are Unix ms).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenameCondition {
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<i64>,
    pub if_unmodified_since: Option<i64>,
}

impl RenameCondition {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Check against the side's current version (`None`: absent). `source`: the rename's
    /// source, which must exist.
    fn check(&self, cur: Option<&Row>, key: &str, source: bool) -> Result<()> {
        let Some(r) = cur else {
            if source || self.if_match.is_some() {
                return Err(StoreError::NoSuchKey(key.to_string()));
            }
            // A destination that doesn't exist wasn't modified since anything.
            if self.if_modified_since.is_some() {
                return Err(StoreError::PreconditionFailed);
            }
            return Ok(());
        };
        if let Some(m) = &self.if_match
            && !etag_matches(m, &r.etag)
        {
            return Err(StoreError::PreconditionFailed);
        }
        if let Some(m) = &self.if_none_match
            && etag_matches(m, &r.etag)
        {
            return Err(StoreError::PreconditionFailed);
        }
        // HTTP dates have whole seconds.
        let modified_s = r.modified_at.div_euclid(1000);
        if let Some(t) = self.if_modified_since
            && modified_s <= t.div_euclid(1000)
        {
            return Err(StoreError::PreconditionFailed);
        }
        if let Some(t) = self.if_unmodified_since
            && modified_s > t.div_euclid(1000)
        {
            return Err(StoreError::PreconditionFailed);
        }
        Ok(())
    }
}

impl Store {
    /// Rename `src` to `dst` in `bucket`, atomically (see the module docs). Returns the object
    /// as it now is at `dst`. Renaming a key to itself changes nothing (its conditions are
    /// still checked).
    pub async fn rename_object(&self, bucket: &str, src: &str, dst: &str, src_cond: &RenameCondition, dst_cond: &RenameCondition) -> Result<ObjectMeta> {
        crate::validate::key(dst)?;
        let now = self.now();
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            let source = rows::resolve(c, &chain, src)?.map(|(r, _)| r);
            src_cond.check(source.as_ref(), src, true)?;
            let source = source.ok_or_else(|| StoreError::NoSuchKey(src.to_string()))?;
            let current = rows::resolve(c, &chain, dst)?.map(|(r, _)| r);
            dst_cond.check(current.as_ref(), dst, false)?;
            if src == dst {
                return source.meta();
            }
            let blob = source.blob()?.to_string();
            // The destination's reference first, so the blob never has none on the way.
            rows::add_ref(c, &blob, 1, now)?;
            let row = Row { key: dst.to_string(), lock: Default::default(), ..source.clone() };
            let stored = versioning::put_key(c, &b, &chain, dst, row, now)?;
            events::emit(c, &b, events::Change::version("ObjectCreated:Copy", &stored), now)?;
            let gone = match Mode::of(&b) {
                // Not to the trash: the bytes moved, they weren't deleted.
                Mode::Off => {
                    let old = rows::write_key(c, &b.id, &chain, b.recover_days, src, None, false, now)?;
                    DeleteOutcome { existed: old.is_some(), version_id: None, delete_marker: false }
                }
                _ => versioning::delete_key(c, &b, &chain, src, now)?,
            };
            events::emit_delete(c, &b, src, &gone, false, "ObjectRemoved", now)?;
            stored.meta()
        })
        .await
    }
}
