# roll nosql — architecture

roll nosql is a wide-column database written in Rust. Its client-visible behaviour is 1:1 with
Apache Cassandra 5.0 and ScyllaDB 2026.x: the CQL native protocol, the CQL language, the system
tables, the drivers' routing extensions, ScyllaDB's DynamoDB-compatible API (Alternator) and
ScyllaDB's REST API (which `nodetool` uses). Applications switch by changing the contact point.

It is built for serverless operation:

- Object storage is the source of truth. Local disk and memory are caches.
- A database that nobody is connected to costs only its S3 bytes. It wakes on connect.
- Branches are zero-copy. A branch of a 2 TB production copy is ready as fast as an empty
  database, and it stores only what it changes.

The pitch is "the same behaviour, zero idle cost, instant branches", not "faster than Scylla".
A shared multi-tenant fleet won't beat dedicated Scylla cores on p99, and we don't claim it.

## Who it is for

1. **Preview, dev and CI copies of a production Cassandra or ScyllaDB cluster.** Today each
   one is three or more large nodes. See [preview-environments.md](preview-environments.md).
2. **New applications that want Cassandra or DynamoDB data models without running a cluster.**
3. **Teams leaving ScyllaDB Open Source**, which became source-available in late 2024.

## Shape

```
     drivers (CQL v3–v5, shard-aware)   cqlsh   AWS SDKs (DynamoDB API)   nodetool (REST)
                    │                      │              │                   │
            ┌───────┴──────────────────────┴──────────────┴───────────────────┴──────┐
            │ nosql-server                                                           │
            │  listeners: CQL (+TLS, SNI), shard-aware CQL, Alternator HTTP, REST    │
            │  Node: tenancy, routing, wake-on-connect, idle unload                  │
            │  sessions, prepared cache, events, tracing, paging                     │
            ├────────────────────────────────────────────────────────────────────────┤
            │ nosql-alternator   DynamoDB JSON API → engine calls                    │
            ├────────────────────────────────────────────────────────────────────────┤
            │ nosql-engine   one Database per tenant                                 │
            │  schema (system_schema), system/virtual tables, auth (roles, grants)   │
            │  statement execution: restrictions, filtering, paging, aggregates,     │
            │  writes, batches, counters, LWT, MVs, 2i, SAI, vector ANN, CDC, UDFs,  │
            │  DESCRIBE, dialects (Cassandra 5 / ScyllaDB)                           │
            ├────────────────────────────────────────────────────────────────────────┤
            │ nosql-storage   LSM per table                                          │
            │  memtables · SSTables (own format) · compaction · tombstones/TTL       │
            │  commit log shipped to S3 · manifests · branches · PITR · block cache  │
            └──────────────────────────────┬─────────────────────────────────────────┘
                                           │ pg_storage::Objects
                                   S3 (or a local directory)

  nosql-control: databases, branches, restore, credentials, settings, metering, /v1/nosql API
  rolld: wires listeners + control API     roll-cli: `roll nosql …`     dashboard: /dashboard/nosql
```

Crates:

| crate | role | depends on |
|---|---|---|
| `nosql-types` | CQL data types: values, ordering, wire serialisation, literals, JSON, Murmur3 tokens, timeuuids | — |
| `nosql-proto` | CQL native protocol v3/v4/v5: frames, v5 segments, compression, every message, bounded decoding. Pure. | types |
| `nosql-cql` | CQL lexer and parser to an AST, every statement of both dialects, ANTLR-shaped syntax errors. Pure. | types |
| `nosql-storage` | the LSM storage engine and its persistence on object storage | types, pg-storage |
| `nosql-engine` | the database: schema, execution, system tables, auth, functions, dialects | types, cql, storage, redis-lua |
| `nosql-alternator` | the DynamoDB API on top of the engine, as ScyllaDB maps it | engine |
| `nosql-server` | networking, tenancy, sessions, Scylla REST API, HTTP data API for the dashboard | proto, engine, alternator |
| `nosql-sstable-import` | reading Cassandra and ScyllaDB SSTable backups (snapshots, ScyllaDB Manager, Medusa) into roll SSTables | storage |
| `nosql-control` | control plane and the `/v1/nosql` API | server, sstable-import |

## Dialects

Cassandra 5.0 and ScyllaDB agree on most of CQL, but not everywhere: error texts, the columns of
`system_schema` and `system.local`, default table options, `DESCRIBE` output, supported protocol
versions, cell reconciliation tie-breaks, and features (SAI, vectors and data masking on
Cassandra; Lua UDFs, CDC, Alternator, service levels and tablets on ScyllaDB).

Each database has a **dialect**, chosen at creation and changeable later:

- `scylla` (default) — identifies as ScyllaDB 2026.3.
- `cassandra` — identifies as Cassandra 5.0.9.

The engine takes the dialect from the database and consults it wherever the two differ. Each
dialect is tested against its own reference image. Features that exist in only one product are
available only in that dialect, with the error the other product gives, unless a setting opts in.

Each database also carries the settings that would be in `cassandra.yaml` / `scylla.yaml` and
change behaviour clients can see:

- `materialized_views_enabled`, `sasi_indexes_enabled`, `user_defined_functions_enabled`
- batch size warn/fail thresholds, tombstone warn/fail thresholds, guardrails
- `authenticator` / `authorizer`, `cluster_name`, datacenter and rack names, `num_tokens`,
  number of shards presented to drivers

Defaults match the reference Docker images. They are visible in `system_views.settings`
(Cassandra) and `system.config` (ScyllaDB).

## The cluster a client sees

A database looks like a healthy single-datacenter cluster:

- **Nodes.** Each endpoint presents one node in `system.local`. `system.peers` and
  `system.peers_v2` are empty. The node has a stable `host_id` derived from the database id and
  its own tokens (`num_tokens`, default as in the image). Datacenter and rack names are settings,
  so a driver configured with `localDc = "us-east"` works against a branch of a cluster whose
  datacenter is `us-east`.
- **Replication.** `SimpleStrategy` and `NetworkTopologyStrategy` keyspaces are accepted with
  the same validation the reference applies. The options are stored and shown verbatim. Every
  consistency level is satisfied, because each partition has exactly one writer and object
  storage is the replication. A request never fails with `Unavailable`.
- **ScyllaDB shards.** In the `scylla` dialect, `SUPPORTED` advertises `SCYLLA_SHARD`,
  `SCYLLA_NR_SHARDS`, the sharding algorithm, `SCYLLA_SHARD_AWARE_PORT`, LWT metadata marking,
  the rate-limit error and (with tablets) `TABLETS_ROUTING_V1`. Shard-aware drivers then open one
  connection per presented shard, chosen by source port on the shard-aware port.
- **Schema agreement.** Every schema change writes a new `schema_version` and pushes
  `SCHEMA_CHANGE` events to connections that registered for them. Drivers' agreement checks pass
  on the first poll.
- **Events.** `TOPOLOGY_CHANGE` and `STATUS_CHANGE` are sent when a database moves between nodes,
  so drivers reconnect cleanly.

## Tenancy and routing

A **node** (`nosql_server::Node`) serves many databases. A connection is routed to a database by:

1. **TLS SNI**: `<db>.<domain>` (hosted endpoints always use TLS on 9042).
2. **The SASL PLAIN user name** `role@db`. The role inside the database is the part before `@`.
3. **Single mode**: one default database, used by the Docker image and `rolld --nosql-single`.
   Auth defaults to `AllowAllAuthenticator`, as in the reference images.

Requests sent before `AUTHENTICATE` completes (`OPTIONS`, `STARTUP`) need no database. A
connection is bound to its database once routing resolves.

**Scale to zero.** A database with no connections and no Alternator traffic for `idle_timeout`
(default 300 s) is unloaded: its memtables are flushed to SSTables, the manifest is written and
memory is released. The next connection loads the manifest and the schema (the system keyspaces
are ordinary tables) and serves at once. Data blocks are fetched lazily.

An unload gives its memory back at once. Running compactions stop at their next partition
(their outputs are discarded), the memtables are flushed (as many at once as the flush
concurrency allows), the database's blocks leave both caches, and the allocator is told to
return freed memory to the OS (`mi_collect`). Idle blocking-pool threads exit after 50 ms,
because memory another thread frees stays with the allocating thread's heap until then.

**Memory budgets** (`nosql_storage::memory`) keep a node's memory bounded whatever its tenants
do. Every one of them is reported by `GET /v1/nosql` (`memory`):

| budget | default (rolld flag) | enforcement |
|---|---|---|
| memtables, node | 512 MiB (`--nosql-memtable-budget`) | Past half of it, the largest active memtables are flushed. At it, a database's writes wait while it holds at least its fair share (the budget divided by the databases holding memtable data). Past the budget plus an eighth, every write waits. |
| memtables, database | half the node's budget, all of it in single mode (`--nosql-memtable-db-cap`); a database's `memtable_space` setting (its plan) overrides it | Past half of it, its largest memtables are flushed. At it, its writes wait. Cassandra's `system_views.settings` shows it as `memtable_heap_space`. |
| write wait | 2 s | Then the write fails with the reference's `WriteTimeout` (Cassandra's `write_request_timeout`), at the statement's consistency. |
| block cache, memory | 256 MiB (`--nosql-cache-memory`) | Shared by all databases, LRU. |
| block cache, disk | 4 GiB (`--nosql-cache-disk`, S3 only) | Shared by all databases, LRU. |
| flushes, compactions | 2 and 2 at once (`--nosql-flush-concurrency`, `--nosql-compaction-concurrency`) | Semaphores. A compaction reads ahead at most 16 MiB of decoded blocks over all its inputs. |
| commit log backlog | 64 MiB | When shipping lags, writes wait, and then time out as above. |
| connections | 256 requests in flight and 256 queued responses per connection | Past either, the connection stops reading. |

Memtable accounting counts what writes pin: the encoded frame the cells reference, and the
row, column and partition structures. `tests/memory.rs` checks it against a counting allocator
(within 0.9–1.4x of the real heap). SSTable index keys are copies, not slices of the blocks or
frames they were read from: a slice would keep a whole decoded block alive for the SSTable's
lifetime.

## Storage

`nosql-storage` is an LSM tree per table. Its data model is Cassandra's storage model:

- A **partition** has a key (the serialised partition key), a token (Murmur3, as Cassandra
  computes it), a partition deletion, a static row, rows in clustering order, and range
  tombstones.
- A **row** has a clustering key, primary-key liveness (the row marker that `INSERT` writes and
  `UPDATE` does not), a row deletion (regular or shadowable, for MVs) and cells.
- A **cell** has a timestamp, an optional TTL with an expiry time, and a value or a tombstone.
  A non-frozen collection or UDT is a complex column: cells with paths plus a complex deletion.
  Counters are cells carrying a clock and a value.

Reconciliation follows the reference exactly: the highest timestamp wins, and ties break by the
dialect's rules (tombstone first, then expiry, then value bytes, in the dialect's order).

**Write path.** A mutation is validated by the engine, applied to the table's memtable, and
appended to the node's commit log buffer. The buffer ships to object storage as a segment every
~100 ms, or sooner when full. Segments hold the frames of every database on the node.

- With `durability = async` (the default), the write is acknowledged once it is in the
  memtable. This bounds data loss on a node crash to the last ship interval, which is stricter
  than Cassandra's default periodic commit log (10 s).
- With `durability = sync`, the write is acknowledged once its segment is stored. Use it with
  S3 Express One Zone for single-digit millisecond writes.

**Flush.** A memtable over its size budget, or any memtable at unload, is written as an
SSTable, uploaded, and recorded in a new manifest version. The manifest's replay position moves
past the flushed frames.

**SSTables** use our own format, designed for range reads from object storage:

- `<id>.data`: partitions in token order, rows in clustering order, in compressed blocks of
  ~64 KiB. Each block can be decoded on its own.
- `<id>.index`: the partition index, row indexes for wide partitions, a bloom filter on
  partition keys, and statistics (min/max timestamps, tombstone counts, min/max clustering, TTL
  and deletion-time histograms for TWCS and tombstone GC). It is small and loaded at open.

Ids are unique and never reused. A compaction always writes new objects.

**Bulk loading.** Imports of SSTable backups (`nosql-sstable-import`) write SSTables outside the
write path, without commit log frames: each source SSTable is streamed, in its token order, into
roll SSTables in a scratch file and uploaded; a named snapshot lists them while the import runs
so garbage collection keeps them; then every table's new SSTables are swapped in with one
manifest version (`nosql_storage::ingest`). Replicas and overlapping SSTables reconcile on read
and compaction as Cassandra's do; only counter tables are merged first, by replica shard.

**Reads** merge the memtables with the SSTables whose bloom filter, token range and clustering
range can hold the data. Blocks come from an in-memory LRU, then a local disk cache, then
coalesced S3 range GETs. Both caches have a byte budget per node.

**Compaction** runs per table in the background. It follows the declared strategy closely
enough that its visible effects match:

- tombstones purged after `gc_grace_seconds` (or ScyllaDB's `tombstone_gc` mode)
- TWCS windows dropped whole when they expire

The options themselves are stored and shown verbatim. The strategies are
`SizeTieredCompactionStrategy`, `LeveledCompactionStrategy`, `TimeWindowCompactionStrategy`,
`IncrementalCompactionStrategy` (ScyllaDB) and `UnifiedCompactionStrategy` (Cassandra 5).

**Object layout:**

```
nosql/log/<node>/<seq>.zst                    commit log segments (all databases on a node)
nosql/db/<db>/manifest.json                   the current manifest
nosql/db/<db>/manifests/<version>.json        older versions, kept for point-in-time restore
nosql/db/<db>/sst/<table-id>/<sst-id>.data    SSTable data, read by range
nosql/db/<db>/sst/<table-id>/<sst-id>.index   SSTable index, filter and statistics
```

A manifest lists, per table, its SSTables (with the owning database's key prefix, so a branch
can reference its parent's objects) and the commit log frames to replay. A garbage collector
deletes objects that no manifest, including retained history, references.

**Point-in-time restore.** Older manifest versions and segments are kept for
`history_retention` (default 7 days). Every frame carries its wall-clock time. To restore to T,
take the last manifest version flushed before T and replay frames up to T. Restore runs in
place, archiving the replaced state, or into a new database.

**Branches.** A branch is a new database whose first manifest is its parent's state at T: the
parent's SSTables by reference, plus the parent's frames up to T to replay. Nothing is copied.
The branch writes only its own objects. Schema, roles and grants come along, because they are
data in the system keyspaces.

## Engine

`nosql-engine` owns one `Database` per tenant. A statement goes through these steps:

1. **Parse.** `nosql-cql` turns the text into an AST. Prepared statements cache it.
2. **Prepare.** Names resolve against the schema and terms are typed and bound, including bind
   markers, `UNSET`, collection literals, UDT and tuple literals, and function calls. Statement
   restrictions are analysed with Cassandra's rules: partition key `=`/`IN`, `token()` ranges,
   clustering prefixes and slices, multi-column relations, `CONTAINS`/`CONTAINS KEY`, `LIKE`,
   index selection, `ALLOW FILTERING`. The errors are the reference's messages.
3. **Authorize** against the role's permissions on the resources the statement touches.
4. **Execute.**
   - **Reads:** partition and slice queries, full and token-range scans, filtering, `DISTINCT`,
     `GROUP BY`, aggregates, `PER PARTITION LIMIT`, `LIMIT`, `ORDER BY`, `ANN OF`, `JSON`,
     writetime, ttl, casts and selector arithmetic. Paging state is opaque to clients and
     resumable on any node.
   - **Writes:** `INSERT`/`UPDATE`/`DELETE` with `TTL`, `TIMESTAMP` and ScyllaDB's `TIMEOUT`,
     collection operations, counters, `UNSET`, and batches with their size warnings.
   - **Lightweight transactions:** serialised per partition by an in-process lock. The result
     shape (`[applied]` plus existing values) matches the reference.
   - **Schema changes:** written to `system_schema` and broadcast as events.
5. **Derived data.** Materialized views, secondary indexes (ScyllaDB global and local indexes,
   Cassandra 2i and SAI), the vector index, and CDC log tables are maintained on the write path
   with each product's semantics, including result ordering through an index.

System and virtual tables are generated from engine state with each dialect's exact columns:
`system`, `system_schema`, `system_auth`, `system_distributed`, `system_traces`, `system_views`,
`system_virtual_schema`, and ScyllaDB's `system.*` additions.

**Functions:**

- every native scalar and aggregate function
- Cassandra 5's math, collection and masking functions
- ScyllaDB's Lua UDFs, through `redis-lua`
- Java UDFs are not supported. Both products disable them by default, and we return the error a
  default-configured server gives.

**Auth.** `PasswordAuthenticator` (SASL PLAIN, bcrypt hashes in `system_auth.roles`) and
`CassandraAuthorizer` semantics. The engine supports:

- roles and grants on every resource kind
- `LIST` statements and the legacy `USER` statements
- `HASHED PASSWORD`
- ScyllaDB service levels
- Cassandra network permissions (`ACCESS TO DATACENTERS`)
- the `UNMASK` and `SELECT_MASKED` permissions

## Alternator

`nosql-alternator` maps the DynamoDB API onto the engine the way ScyllaDB does, so CQL and
DynamoDB clients see the same tables, byte for byte (`crates/nosql-server/tests/alternator_compat.rs`
compares the CQL view of both against the reference):

- A table `T` is CQL table `alternator_T.T` in keyspace `alternator_T` (tablets unless the
  `system:initial_tablets` tag says otherwise). Key attributes become columns and the rest go in
  the `:attrs` map; tags, TTL and provisioned throughput live in the `scylla_tags` extension.
- GSIs and LSIs are materialized views (`T:gsi`, `T!:lsi`) keyed by columns computed from
  `:attrs`, maintained and backfilled by the engine's view machinery; Streams are the CDC log,
  one shard per CDC stream; TTL is a background expiration scanner; `.scylla.alternator.KS.T`
  reads the engine's system tables.
- The API covers items, queries and scans with every expression language, batches, GSIs and
  LSIs, TTL, tags, DynamoDB Streams, vector indexes and the ScyllaDB-specific features (write
  isolation, internal tables, live `system.config` options, Prometheus metrics).
- Requests are authenticated with SigV4: the access key id is a role name and the secret is that
  role's salted hash, as in ScyllaDB. With `CassandraAuthorizer`, operations need the CQL
  permissions ScyllaDB asks for (SELECT, MODIFY, CREATE, DROP, ALTER), creators are granted what
  they create and drops revoke.
- Hosted endpoints route by `Host` (`<db>.alternator.<domain>`) or by the access key
  `role@<db>`; every request holds a lease, so it wakes the database and keeps it loaded like a
  CQL connection.

## Scylla REST API

`nosql-server` serves the subset of ScyllaDB's REST API that `nodetool` (ScyllaDB's native
nodetool) and test suites use. It covers:

- status, info, ring, describecluster
- flush, compact, cleanup, snapshots
- tablestats and tablehistograms, settraceprobability
- compaction throughput and similar no-ops that report success

Locally it listens on its own port. Hosted, it is reached through the roll API with an API key.
Cassandra's JMX `nodetool` is not supported. The CLI has equivalents.

## Control plane

`nosql-control` follows `redis-control`:

- It keeps its own SQLite file with tables for databases, branches and usage.
- It runs a `Node` with a catalog backed by that store, and applies settings live.
- It meters storage, request units and connection-seconds, and estimates cost against AWS
  prices and the managed alternatives (ScyllaDB Cloud, Amazon Keyspaces, DynamoDB, Astra).
- It serves `/v1/nosql` (see [contracts.md](contracts.md)).

`rolld` merges the router and runs the listeners. `roll nosql …` and `/dashboard/nosql` use the
API.

## Performance

- **Database shards.** Data is split by token into shards. Each shard has its own memtable and
  its own write lock. Reads never take a lock that a flush or compaction holds for long.
- **Execution.** Execution is async on tokio, so a read never blocks a thread on S3. CPU-heavy
  work (block decode, compaction, merges) runs on a bounded blocking pool.
- **Values.** Values stay serialised (`Bytes`) end to end. A read sends cells to the client
  without re-encoding them.
- **Prepared statements** cache their plan, not just their AST.
- **Benchmarks.** These live in `docs/benchmarks/nosql.md`. They run `cassandra-stress`,
  `scylla-bench` and `latte` against roll and both references on the same machine.
