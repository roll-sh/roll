# pg-engine

roll's own Postgres-compatible database. One process serves many databases. A database that
nobody is using has no process, no memory and no CPU: it is a manifest and some immutable
files in object storage, plus whatever is still cached on local disk.

```
client ──pg wire / http──▶ server ──▶ session ──▶ SQL (pg_query → binder → planner → executor)
                                                         │
                                         catalog ◀───────┤
                                                         ▼
                                              MVCC transactions (locks, snapshots)
                                                         │
                                              LSM: WAL → memtable → SSTs
                                                         │
                                         local disk cache ◀──▶ object storage (S3)
```

## Compatibility strategy

- **Parser.** `pg_query` embeds Postgres's real grammar, so anything Postgres can parse we can
  parse. Unsupported constructs fail with `0A000 feature_not_supported`, never with a wrong
  answer.
- **Semantics.** Types, casts, operators, functions, error codes and messages follow Postgres.
  Text output (`DateStyle ISO`, `IntervalStyle postgres`, `extra_float_digits 1`) is
  byte-for-byte what Postgres prints.
- **Proof.** Every `tests/slt/*.slt` file runs against real Postgres 18 as well as against the
  engine (`ROLL_SLT_PG=1`). A test that only passes on the engine is a bad test.
- **Collation.** Text compares bytewise (the `C` collation). The reference Postgres runs with
  `--locale=C` so ordering matches.

## Storage

- **Keys.** Order-preserving encoding: `table_oid | index_id | key columns`. Tables without a
  primary key get a hidden `rowid`. Secondary indexes store `index columns | pk`.
- **Versions.** Each stored key is `user_key | !commit_ts`, so the newest version sorts first
  and a snapshot read is one seek.
- **LSM.** Commits append to a WAL (group commit, fsync) and go into a memtable. Full
  memtables are written as SSTs (sorted blocks, zstd, bloom filter, block index). Compaction
  merges SSTs and drops versions no snapshot can see.
- **Bottomless.** SSTs are immutable, so they are uploaded once and shared. The manifest
  names the live SSTs and the WAL position. A branch is a new manifest that points at the
  parent's SSTs: instant, zero-copy. Point-in-time restore replays WAL up to a timestamp.
- **Idle.** A database with no sessions is closed after a timeout: memtable flushed, files
  synced, memory released. Opening it reads the manifest; blocks load on demand.

## Transactions

- One owner node per database, so ordering and locks are in memory (a cluster spreads
  databases across nodes; ranges/sharding come later).
- Snapshot reads at a commit timestamp. `READ COMMITTED` (the default) takes a snapshot per
  statement; `REPEATABLE READ` per transaction. `SERIALIZABLE` adds read tracking (SSI).
- Writes are buffered in the transaction and applied atomically at commit.
- Row locks (`UPDATE`, `DELETE`, `SELECT … FOR UPDATE`, unique keys) live in a lock manager
  with deadlock detection. Under `READ COMMITTED`, a writer that waited re-reads the newest
  row and re-checks its `WHERE` (Postgres's EvalPlanQual).
- DDL is transactional: the catalog is stored in the same MVCC keyspace.

## Layout

| module | role |
| --- | --- |
| `types` | data types, values, text/binary I/O, casts, numeric, date/time, json |
| `storage` | key encoding, WAL, memtable, SST, manifest, object storage |
| `txn` | MVCC store, snapshots, lock manager |
| `catalog` | schemas, tables, indexes, sequences; `pg_catalog` / `information_schema` views |
| `sql` | binder (pg_query AST → plan), expressions, functions, planner |
| `exec` | executor |
| `session` | per-connection state, prepared statements, portals, settings |
| `server` | wire protocol, auth (SCRAM-SHA-256) |
| `engine` | database registry: open on demand, close when idle |
