//! The metadata schema. Ids are stored as raw bytes; every table is keyed by repo first so a
//! repo can be dropped with one range delete per table.
//!
//! Written once in SQLite's dialect; `code_sql` translates it for Postgres (BIGINT, BYTEA,
//! `TEXT COLLATE "C"`, an identity column for `land_log.seq`).

use crate::ChangesetRow;
use crate::ids::{ChangesetId, Oid};
use bytes::Bytes;
use code_sql::Conn;
use code_sql::ValueRef::{self, Blob, Int, Text};
use std::collections::HashMap;

pub const TABLES: &[&str] =
    &["changesets", "cs_git", "git_commits", "trees", "git_raw", "blobs", "refs", "path_index", "land_stats", "land_log", "repos"];

pub const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS repos (
        repo TEXT PRIMARY KEY,
        created_at INTEGER NOT NULL
    );
    -- Changesets (Bonsai). generation = max(parent gens) + 1; depth = first-parent depth;
    -- skip = first-parent ancestor at depth (depth & (depth - 1)).
    CREATE TABLE IF NOT EXISTS changesets (
        repo TEXT NOT NULL,
        id BLOB NOT NULL,
        generation INTEGER NOT NULL,
        depth INTEGER NOT NULL,
        skip BLOB,
        data BLOB NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (repo, id)
    ) WITHOUT ROWID;
    -- Derived git data, both directions.
    CREATE TABLE IF NOT EXISTS cs_git (
        repo TEXT NOT NULL,
        cs BLOB NOT NULL,
        commit_oid BLOB NOT NULL,
        tree_oid BLOB NOT NULL,
        PRIMARY KEY (repo, cs)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS git_commits (
        repo TEXT NOT NULL,
        oid BLOB NOT NULL,
        cs BLOB NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (repo, oid)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS trees (
        repo TEXT NOT NULL,
        oid BLOB NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (repo, oid)
    ) WITHOUT ROWID;
    -- Git objects that aren't derived from changesets (annotated tags).
    CREATE TABLE IF NOT EXISTS git_raw (
        repo TEXT NOT NULL,
        oid BLOB NOT NULL,
        kind INTEGER NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (repo, oid)
    ) WITHOUT ROWID;
    -- Blobs live in object storage; this records which a repo has, and their sizes.
    CREATE TABLE IF NOT EXISTS blobs (
        repo TEXT NOT NULL,
        oid BLOB NOT NULL,
        size INTEGER NOT NULL,
        PRIMARY KEY (repo, oid)
    ) WITHOUT ROWID;
    -- A ref update is a compare-and-swap on one row. tag = annotated tag object, if any.
    -- Bases older than floor_gen skip the path index (it was reset by a non-fast-forward move).
    CREATE TABLE IF NOT EXISTS refs (
        repo TEXT NOT NULL,
        name TEXT NOT NULL,
        cs BLOB NOT NULL,
        tag BLOB,
        version INTEGER NOT NULL,
        floor_gen INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (repo, name)
    ) WITHOUT ROWID;
    -- Per ref: the generation of the changeset that last changed each path.
    CREATE TABLE IF NOT EXISTS path_index (
        repo TEXT NOT NULL,
        ref TEXT NOT NULL,
        path TEXT NOT NULL,
        generation INTEGER NOT NULL,
        PRIMARY KEY (repo, ref, path)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS land_stats (
        repo TEXT NOT NULL,
        minute INTEGER NOT NULL,
        count INTEGER NOT NULL,
        PRIMARY KEY (repo, minute)
    ) WITHOUT ROWID;
    -- Every published landing, in order, written in the landing's own transaction: the
    -- durable feed consumers that must not miss one (webhooks) read instead of the
    -- broadcast channel. Kept for LAND_LOG_RETENTION_MS.
    CREATE TABLE IF NOT EXISTS land_log (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        at_ms INTEGER NOT NULL,
        record TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS land_log_repo ON land_log (repo);
";

/// SQLite reader connections.
pub const READERS: usize = 8;
/// The SQLite writer's page cache, in KiB.
pub const CACHE_KIB: i64 = 64 << 10;

/// Rows most statements insert at once.
const ROWS_PER_INSERT: usize = 64;

/// The statements of one write, built before it (a write may run more than once) and sent in
/// one round trip on Postgres ([`code_sql::Conn::execute_pipeline`]). Inserts put several rows
/// in each statement: on Postgres a statement costs far more than a row, even pipelined.
#[derive(Default)]
pub(crate) struct Steps<'a>(Vec<(String, Vec<Vec<ValueRef<'a>>>)>);

impl<'a> Steps<'a> {
    /// One statement.
    pub fn push(&mut self, sql: &str, params: Vec<ValueRef<'a>>) {
        self.0.push((sql.to_string(), vec![params]));
    }

    /// `head VALUES (…), (…) … tail` for `rows`, in chunks of 64, then 32, 16, … 1, so only a
    /// few statement shapes exist (each is prepared once per connection). Rows must not
    /// conflict with each other under `ON CONFLICT … DO UPDATE`.
    pub fn insert(&mut self, head: &str, tail: &str, rows: Vec<Vec<ValueRef<'a>>>) {
        let Some(cols) = rows.first().map(Vec::len) else { return };
        let group = format!("({})", vec!["?"; cols].join(", "));
        let mut rest = &rows[..];
        while !rest.is_empty() {
            let k = if rest.len() >= ROWS_PER_INSERT { ROWS_PER_INSERT } else { 1 << rest.len().ilog2() };
            let (chunk, r) = rest.split_at(k);
            let sql = format!("{head} VALUES {}{tail}", vec![group.as_str(); k].join(", "));
            match self.0.last_mut() {
                // The same shape again (a run of full chunks): one more parameter list.
                Some((last, lists)) if *last == sql => lists.push(chunk.concat()),
                _ => self.0.push((sql, vec![chunk.concat()])),
            }
            rest = r;
        }
    }

    pub fn changesets(&mut self, repo: &'a str, rows: &'a [ChangesetRow]) {
        self.insert(
            "INSERT OR IGNORE INTO changesets (repo, id, generation, depth, skip, data, created_at)",
            "",
            rows.iter()
                .map(|m| {
                    let skip = m.skip.as_ref().map_or(ValueRef::Null, |s| Blob(&s.0));
                    vec![Text(repo), Blob(&m.id.0), Int(m.generation as i64), Int(m.depth as i64), skip, Blob(&m.data), Int(m.created_at)]
                })
                .collect(),
        );
    }

    pub fn trees(&mut self, repo: &'a str, trees: impl IntoIterator<Item = (&'a Oid, &'a Bytes)>) {
        self.insert("INSERT OR IGNORE INTO trees (repo, oid, data)", "", trees.into_iter().map(|(o, d)| vec![Text(repo), Blob(&o.0), Blob(d)]).collect());
    }

    /// Which blobs the repo has (uploaded before the write).
    pub fn blobs(&mut self, repo: &'a str, blobs: &'a HashMap<Oid, Bytes>) {
        self.insert(
            "INSERT OR IGNORE INTO blobs (repo, oid, size)",
            "",
            blobs.iter().map(|(o, d)| vec![Text(repo), Blob(&o.0), Int(d.len() as i64)]).collect(),
        );
    }

    /// Derived git data, both directions: `(changeset, commit, tree, commit bytes)`.
    pub fn git(&mut self, repo: &'a str, rows: &'a [(ChangesetId, Oid, Oid, Bytes)]) {
        self.insert(
            "INSERT OR IGNORE INTO cs_git (repo, cs, commit_oid, tree_oid)",
            "",
            rows.iter().map(|(cs, commit, tree, _)| vec![Text(repo), Blob(&cs.0), Blob(&commit.0), Blob(&tree.0)]).collect(),
        );
        self.insert(
            "INSERT OR IGNORE INTO git_commits (repo, oid, cs, data)",
            "",
            rows.iter().map(|(cs, commit, _, data)| vec![Text(repo), Blob(&commit.0), Blob(&cs.0), Blob(data)]).collect(),
        );
    }

    /// Run them, in order (inside a write).
    pub fn run(&self, c: &Conn<'_>) -> code_sql::Result<()> {
        let steps: Vec<(&str, Vec<Vec<ValueRef<'_>>>)> = self.0.iter().map(|(sql, lists)| (sql.as_str(), lists.clone())).collect();
        c.execute_pipeline(&steps)?;
        Ok(())
    }
}
