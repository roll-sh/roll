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
