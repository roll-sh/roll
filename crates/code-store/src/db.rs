//! The metadata schema. Ids are stored as raw bytes; every table is keyed by repo first so a
//! repo can be dropped with one range delete per table.
//!
//! Written once in SQLite's dialect; `code_sql` translates it for Postgres (BIGINT, BYTEA,
//! `TEXT COLLATE "C"`, an identity column for `land_log.seq`).

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
