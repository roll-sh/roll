# roll nosql — preview environments

The main reason roll nosql exists is to give every pull request, developer and CI run its own
copy of a production Cassandra or ScyllaDB database. Today that copy is three or more large
nodes. On roll it is free while idle, ready in about a second, and holds only what it changes.

This page describes the experience end to end. Every feature here has an API route, a
`roll nosql` command and a dashboard control.

## Get production data in

A branch needs a parent. These are the ways to get production data into roll. They are listed
from least to most effort:

1. **Run on roll.** Production itself is a roll database, so branches come from it directly.
2. **Mirror a live cluster.** `roll nosql import --from cql://user:pass@prod-host:9042
   --keyspaces app,events [--every 24h]` runs a parallel token-range scan over CQL and loads
   the result as SSTables. Writetimes and TTLs are kept, so data reads exactly as it does in
   production.
   - It needs only a read-only role on the source.
   - With `--every`, it refreshes a mirror database on a schedule. Each refresh is a new
     manifest version, so branches can be taken from any refresh.
3. **Restore from backups.** `roll nosql import --from s3://bucket/path [--every 24h]` reads
   backups made by `nodetool snapshot`, ScyllaDB Manager or Medusa straight from S3 (or a local
   directory, `file:///path`), and needs no source cluster at all.
   - It reads Cassandra `nb` (4.x, and 5.0 in its default compatibility mode), `oa` and BTI
     `da` (5.0), and ScyllaDB `me`, `ms` and `mt` SSTables, with any of the products'
     compressors (LZ4, Snappy, Deflate, Zstd, and ScyllaDB's dictionary compressors), checking
     every chunk checksum and digest.
   - Write times, TTLs, tombstones, counters, collections, static rows and range tombstones are
     kept as stored, so the data reads exactly as it did on the source. Every node's SSTables
     are imported and reconcile like replicas (counters merge per replica shard).
   - The newest snapshot is used; `?tag=` (snapshots, Manager) or `?backup=` (Medusa) picks
     another, `?cluster=` a Manager cluster, `?region=` and `?endpoint=` the bucket's.
   - The data lands as SSTables, swapped in with one manifest version: a branch can be taken
     right after. With `--every`, each refresh re-reads the newest snapshot and replaces the
     previous one's data.
   - Indexes and materialized views are rebuilt from the imported rows.

Schema comes along in every case: the system keyspaces are rebuilt from the source's
`DESCRIBE`, from the snapshot's `schema.cql` (user types also from the SSTables' own type
names), or from ScyllaDB Manager's and Medusa's schema files. A keyspace whose definition isn't
in the backup gets `replication_factor` 1.

## Branch per pull request

```sh
roll nosql branch app-prod --name pr-482 --expires 72h
roll nosql branch app-prod --name pr-482 --at "2026-10-20 08:00"   # from a point in time
roll nosql branch app-prod --name pr-482 --schema-only             # schema and roles, no rows
```

- **Instant and zero-copy.** A branch references its parent's SSTables. Creating one writes a
  single manifest, whatever the size of the parent.
- **Same credentials, same names.** Roles, passwords, grants, keyspace names, the cluster name,
  and datacenter and rack names all come from the parent. The application changes only its
  contact point: `pr-482-<id>.nosql.<domain>:9042`. A driver configured with
  `localDc = "us-east-1"` works unchanged.
- **Stable endpoint.** `roll nosql reset pr-482` re-branches from the parent's latest state, or
  from `--at`, while keeping the branch id, endpoint and credentials. A preview deployment
  never needs new config.
- **Expiry.** `--expires` deletes the branch when it runs out. The API also takes
  `expires_at`, so CI and GitHub integrations can delete a branch when its pull request
  closes.
- **Cost.** Branches scale to zero after `idle_timeout` and wake on connect. A branch pays only
  for the data it writes and the time it is in use.

## Safe data

- **Masking.** A branch can carry a masking policy, as column → mask function using Cassandra
  5's masking functions (`mask_default`, `mask_null`, `mask_inner`, `mask_outer`, `mask_hash`,
  `mask_replace`). It is applied at read time for every role, in both dialects. The stored data
  is the parent's untouched objects. Policies are set once on the parent and inherited by its
  branches, so nobody forgets them.
- **Read-only branches.** A branch can be marked read-only for safe production debugging.
  Writes return `Unauthorized` with a clear message.

## Review migrations

- `roll nosql diff pr-482` prints the schema difference between a branch and its parent as CQL
  (`CREATE`/`ALTER`/`DROP`), generated from both sides' `DESCRIBE` output. The dashboard shows
  the diff on the branch page and in the pull request comment.
- `roll nosql exec pr-482 -f migrations/0042.cql` runs a CQL file, as `cqlsh -f` would, and
  supports `SOURCE`.
- Migration tools (`golang-migrate`, `cassandra-migrate`, `cqlmigrate`, Liquibase) work
  unchanged.

## Local development and CI

- **Docker image.** `roll/nosql` is a drop-in for `cassandra:5.0` and `scylladb/scylla`:
  - It starts in well under a second. Cassandra takes about 30 s and ScyllaDB about 10 s.
  - It listens on 9042, 19042, 8000 (Alternator) and 10000 (REST).
  - It honours the images' environment variables (`CASSANDRA_CLUSTER_NAME`, `CASSANDRA_DC`,
    `CASSANDRA_RACK`, `CASSANDRA_ENDPOINT_SNITCH`) and ScyllaDB's flags (`--smp`,
    `--developer-mode`, `--overprovisioned`, `--alternator-port`).
  - It prints the log lines Testcontainers waits for.
  - [local-dev.md](local-dev.md) has the full configuration mapping, compose and Testcontainers
    snippets.
- **CI.** Tests can create a branch per job through the API, or run the image as a service
  container. The image can start from a branch snapshot (`ROLL_NOSQL_FROM=<api url>/<branch>`)
  so CI tests run on realistic data.

## Inspect and debug

- **Dashboard.** The `/dashboard/nosql` pages provide:
  - a schema browser (keyspaces, tables, types, views, indexes, with `DESCRIBE` output)
  - a table data browser with paging
  - a CQL console with completion, tracing and `DESCRIBE`
  - per-branch usage and cost
  - slow queries, tombstone and batch-size warnings, and connected clients
- **CLI.** `roll nosql cqlsh <db>` is a built-in CQL shell with cqlsh's output format. No
  Python is needed.
- **Status.** `roll nosql status|tablestats|flush|compact <db>` are the `nodetool` equivalents.
  ScyllaDB's own `nodetool` works against the REST port.
- **Driver snippets.** Connection snippets for every driver come with the right datacenter,
  TLS and SNI settings.
