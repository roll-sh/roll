//! Metadata schema. Every table has a primary key (SQLite sessions only record those). Log
//! entries carry the schema version they were written with; replay migrates before applying
//! newer ones, so migrations must stay deterministic and append-only.

use crate::error::Result;
use rusqlite::Connection;

pub(crate) const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE buckets (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, created_at INTEGER NOT NULL,
  public INTEGER NOT NULL, recover_days INTEGER NOT NULL, cors TEXT, lifecycle TEXT,
  parent_id TEXT, parent_at INTEGER,
  objects INTEGER NOT NULL, bytes INTEGER NOT NULL, trash_bytes INTEGER NOT NULL
);
CREATE INDEX buckets_parent ON buckets (parent_id) WHERE parent_id IS NOT NULL;

CREATE TABLE objects (
  bucket_id TEXT NOT NULL, key TEXT NOT NULL, blob_id TEXT,
  size INTEGER NOT NULL, etag TEXT NOT NULL, content_type TEXT, cache_control TEXT,
  content_disposition TEXT, content_encoding TEXT, content_language TEXT, expires TEXT,
  metadata TEXT NOT NULL, tags TEXT NOT NULL,
  checksum_algorithm TEXT, checksum_type TEXT, checksum TEXT,
  parts INTEGER NOT NULL, part_sizes TEXT NOT NULL, modified_at INTEGER NOT NULL,
  PRIMARY KEY (bucket_id, key)
) WITHOUT ROWID;

CREATE TABLE trash (
  bucket_id TEXT NOT NULL, key TEXT NOT NULL, deleted_at INTEGER NOT NULL, blob_id TEXT NOT NULL,
  size INTEGER NOT NULL, etag TEXT NOT NULL, content_type TEXT, cache_control TEXT,
  content_disposition TEXT, content_encoding TEXT, content_language TEXT, expires TEXT,
  metadata TEXT NOT NULL, tags TEXT NOT NULL,
  checksum_algorithm TEXT, checksum_type TEXT, checksum TEXT,
  parts INTEGER NOT NULL, part_sizes TEXT NOT NULL, modified_at INTEGER NOT NULL,
  PRIMARY KEY (bucket_id, key, deleted_at)
) WITHOUT ROWID;
CREATE INDEX trash_age ON trash (deleted_at);

CREATE TABLE blobs (
  id TEXT PRIMARY KEY, size INTEGER NOT NULL, crc32c INTEGER NOT NULL, refs INTEGER NOT NULL,
  created_at INTEGER NOT NULL, unreferenced_at INTEGER
) WITHOUT ROWID;
CREATE INDEX blobs_unreferenced ON blobs (unreferenced_at) WHERE refs = 0;

CREATE TABLE uploads (
  id TEXT PRIMARY KEY, bucket_id TEXT NOT NULL, key TEXT NOT NULL, blob_id TEXT NOT NULL,
  backend_upload_id TEXT NOT NULL, content_type TEXT, cache_control TEXT,
  content_disposition TEXT, content_encoding TEXT, content_language TEXT, expires TEXT,
  metadata TEXT NOT NULL, tags TEXT NOT NULL, checksum_algorithm TEXT, checksum_type TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX uploads_key ON uploads (bucket_id, key, created_at);
CREATE INDEX uploads_blob ON uploads (blob_id);

CREATE TABLE parts (
  upload_id TEXT NOT NULL, part_number INTEGER NOT NULL, size INTEGER NOT NULL, etag TEXT NOT NULL,
  checksum TEXT, backend_etag TEXT NOT NULL, created_at INTEGER NOT NULL, crc32c INTEGER NOT NULL,
  PRIMARY KEY (upload_id, part_number)
) WITHOUT ROWID;

CREATE TABLE keys (
  id TEXT PRIMARY KEY, secret TEXT NOT NULL, name TEXT NOT NULL, permission TEXT NOT NULL,
  buckets TEXT, created_at INTEGER NOT NULL, last_used_at INTEGER
);

CREATE TABLE usage (
  bucket_id TEXT NOT NULL, day TEXT NOT NULL, class_a INTEGER NOT NULL, class_b INTEGER NOT NULL,
  bytes_in INTEGER NOT NULL, egress_internet INTEGER NOT NULL, egress_internal INTEGER NOT NULL,
  byte_ms REAL NOT NULL,
  PRIMARY KEY (bucket_id, day)
) WITHOUT ROWID;

CREATE TABLE state (k TEXT PRIMARY KEY, v TEXT NOT NULL);

CREATE TABLE orphans (blob_key TEXT PRIMARY KEY, size INTEGER NOT NULL, found_at INTEGER NOT NULL);
"#,
// Bucket configuration (tagging, policy, ACL, website, logging, …) and per-object ACLs and
// website redirects. `id` is '' for one-per-bucket settings, else the configuration's id
// (analytics, metrics, inventory, intelligent-tiering). See `config.rs`.
r#"
CREATE TABLE bucket_config (
  bucket_id TEXT NOT NULL, kind TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL,
  PRIMARY KEY (bucket_id, kind, id)
) WITHOUT ROWID;
ALTER TABLE objects ADD COLUMN acl TEXT;
ALTER TABLE objects ADD COLUMN website_redirect TEXT;
ALTER TABLE trash ADD COLUMN acl TEXT;
ALTER TABLE trash ADD COLUMN website_redirect TEXT;
ALTER TABLE uploads ADD COLUMN acl TEXT;
ALTER TABLE uploads ADD COLUMN website_redirect TEXT;
"#,
// Versioning, Object Lock and noncurrent versions. Every version row carries its version id
// (`vid`, sorting newest first within a key), whether it's the "null" version, whether it's a
// delete marker, and its lock. Existing rows become null versions with a vid from their time.
r#"
ALTER TABLE buckets ADD COLUMN versioning TEXT;
ALTER TABLE buckets ADD COLUMN object_lock TEXT;
ALTER TABLE buckets ADD COLUMN versions INTEGER NOT NULL DEFAULT 0;
ALTER TABLE buckets ADD COLUMN version_bytes INTEGER NOT NULL DEFAULT 0;

ALTER TABLE objects ADD COLUMN vid TEXT NOT NULL DEFAULT '';
ALTER TABLE objects ADD COLUMN is_null INTEGER NOT NULL DEFAULT 1;
ALTER TABLE objects ADD COLUMN delete_marker INTEGER NOT NULL DEFAULT 0;
ALTER TABLE objects ADD COLUMN lock_mode TEXT;
ALTER TABLE objects ADD COLUMN lock_until INTEGER;
ALTER TABLE objects ADD COLUMN legal_hold INTEGER;
UPDATE objects SET vid = printf('%016llx', 9223372036854775807 - modified_at) || '0000000000000000' WHERE blob_id IS NOT NULL;

ALTER TABLE trash ADD COLUMN vid TEXT NOT NULL DEFAULT '';
ALTER TABLE trash ADD COLUMN is_null INTEGER NOT NULL DEFAULT 1;
ALTER TABLE trash ADD COLUMN delete_marker INTEGER NOT NULL DEFAULT 0;
ALTER TABLE trash ADD COLUMN lock_mode TEXT;
ALTER TABLE trash ADD COLUMN lock_until INTEGER;
ALTER TABLE trash ADD COLUMN legal_hold INTEGER;
UPDATE trash SET vid = printf('%016llx', 9223372036854775807 - modified_at) || '0000000000000000';

ALTER TABLE uploads ADD COLUMN lock_mode TEXT;
ALTER TABLE uploads ADD COLUMN lock_until INTEGER;
ALTER TABLE uploads ADD COLUMN legal_hold INTEGER;

CREATE TABLE versions (
  bucket_id TEXT NOT NULL, key TEXT NOT NULL, noncurrent_at INTEGER NOT NULL, blob_id TEXT,
  size INTEGER NOT NULL, etag TEXT NOT NULL, content_type TEXT, cache_control TEXT,
  content_disposition TEXT, content_encoding TEXT, content_language TEXT, expires TEXT,
  metadata TEXT NOT NULL, tags TEXT NOT NULL,
  checksum_algorithm TEXT, checksum_type TEXT, checksum TEXT,
  parts INTEGER NOT NULL, part_sizes TEXT NOT NULL, modified_at INTEGER NOT NULL,
  vid TEXT NOT NULL, is_null INTEGER NOT NULL, delete_marker INTEGER NOT NULL,
  lock_mode TEXT, lock_until INTEGER, legal_hold INTEGER, acl TEXT, website_redirect TEXT,
  PRIMARY KEY (bucket_id, key, vid)
) WITHOUT ROWID;
CREATE INDEX versions_age ON versions (bucket_id, noncurrent_at);

CREATE INDEX objects_blob ON objects (blob_id) WHERE blob_id IS NOT NULL;
CREATE INDEX versions_blob ON versions (blob_id) WHERE blob_id IS NOT NULL;
CREATE INDEX trash_blob ON trash (blob_id);
"#,
// Server-side encryption (SSE-C, SSE-KMS) of a blob, done by the backing object store. No
// row: the backing bucket's default. Keyed by blob: a blob's encryption never changes, and
// every row sharing the blob (copies, branches, the trash) shares it. The row is written with
// the upload's or the blob's own row and removed with them. Never holds a customer key, only
// its MD5.
r#"
CREATE TABLE blob_sse (
  blob_id TEXT PRIMARY KEY,
  algorithm TEXT NOT NULL,
  customer_key_md5 TEXT,
  kms_key_id TEXT, kms_context TEXT, bucket_key INTEGER,
  created_at INTEGER NOT NULL
) WITHOUT ROWID;
"#,
// Event notifications and replication (`events.rs`): the outbox, one row per delivery, written
// in the same transaction as the change it reports (`seq` comes from a counter in `state`, so
// it's never reused), and each version's replication status.
r#"
CREATE TABLE events (
  seq INTEGER PRIMARY KEY, bucket_id TEXT NOT NULL, key TEXT NOT NULL, dest TEXT NOT NULL,
  kind TEXT NOT NULL, body TEXT NOT NULL, created_at INTEGER NOT NULL,
  attempts INTEGER NOT NULL, next_at INTEGER NOT NULL, error TEXT
);
CREATE INDEX events_due ON events (next_at);
CREATE INDEX events_order ON events (dest, key, seq);
CREATE INDEX events_bucket ON events (bucket_id, key);
CREATE TABLE replication_status (
  bucket_id TEXT NOT NULL, key TEXT NOT NULL, version_id TEXT NOT NULL, status TEXT NOT NULL,
  PRIMARY KEY (bucket_id, key, version_id)
) WITHOUT ROWID;
"#,
// Roles for temporary credentials (STS AssumeRole, AssumeRoleWithWebIdentity and the control
// plane's sessions). Sessions themselves are never stored: their tokens are sealed claims (see
// `sts.rs`). `generation` goes up on every change and `revoked_before` (Unix ms) on a
// revocation; either invalidates the role's existing sessions. The session-token keys live in
// `state` (`sts_session_keys`).
r#"
CREATE TABLE roles (
  name TEXT PRIMARY KEY, id TEXT NOT NULL, description TEXT NOT NULL, permission TEXT NOT NULL,
  buckets TEXT, policy TEXT, trust TEXT NOT NULL, max_session_s INTEGER NOT NULL,
  generation INTEGER NOT NULL, revoked_before INTEGER NOT NULL,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
) WITHOUT ROWID;
"#,
// GetObjectTorrent's cache (`torrent.rs`): a blob's BitTorrent piece hashes, kept by blob since
// blobs never change, and removed with the blob's row.
r#"
CREATE TABLE blob_torrent (
  blob_id TEXT PRIMARY KEY, piece_length INTEGER NOT NULL, pieces BLOB NOT NULL,
  created_at INTEGER NOT NULL
);
"#,
];

pub(crate) const LATEST: u32 = MIGRATIONS.len() as u32;

/// Run the migrations after the database's version, up to `version`.
pub(crate) fn migrate(conn: &Connection, version: u32) -> Result<()> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().take(version as usize).skip(current as usize) {
        conn.execute_batch(&format!("BEGIN; {sql}; PRAGMA user_version = {}; COMMIT;", i + 1))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Objects from before versioning become null versions whose id sorts by their time, like
    /// the ids new versions get.
    #[test]
    fn existing_rows_become_null_versions() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c, 1).unwrap();
        c.execute_batch(
            "INSERT INTO buckets (id, name, region, created_at, public, recover_days, objects, bytes, trash_bytes) VALUES ('b', 'b', 'r', 0, 0, 7, 1, 1, 1);
             INSERT INTO objects (bucket_id, key, blob_id, size, etag, metadata, tags, parts, part_sizes, modified_at) VALUES ('b', 'k', 'x', 1, 'e', '{}', '{}', 0, '[]', 1700000000000);
             INSERT INTO objects (bucket_id, key, blob_id, size, etag, metadata, tags, parts, part_sizes, modified_at) VALUES ('b', 't', NULL, 0, '', '{}', '{}', 0, '[]', 5);
             INSERT INTO trash (bucket_id, key, deleted_at, blob_id, size, etag, metadata, tags, parts, part_sizes, modified_at) VALUES ('b', 'k', 9, 'y', 1, 'e', '{}', '{}', 0, '[]', 1600000000000);",
        )
        .unwrap();
        migrate(&c, LATEST).unwrap();
        let q = |sql: &str| c.query_row(sql, [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))).unwrap();
        let (vid, is_null, marker) = q("SELECT vid, is_null, delete_marker FROM objects WHERE key = 'k'");
        assert_eq!((is_null, marker), (1, 0));
        assert_eq!(&vid[..16], &crate::ids::version_id(1_700_000_000_000)[..16], "the same time prefix new ids get");
        assert_eq!(vid.len(), 32);
        assert!(crate::ids::version_id(1_700_000_000_001) < vid, "later versions sort first");
        assert_eq!(q("SELECT vid, is_null, delete_marker FROM objects WHERE key = 't'").0, "", "tombstones aren't versions");
        assert!(q("SELECT vid, is_null, delete_marker FROM trash").0 > vid, "older trash sorts after");
        let (versioning, versions, bytes) = c.query_row("SELECT coalesce(versioning, ''), versions, version_bytes FROM buckets", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))).unwrap();
        assert_eq!((versioning.as_str(), versions, bytes), ("", 0, 0));
    }
}
