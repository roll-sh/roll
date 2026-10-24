# roll nosql — compatibility

The goal is 1:1 client-visible compatibility. If an application, driver, tool or test suite can
observe a difference from the reference, that difference is a bug, unless it is listed under
[Out of scope](#out-of-scope).

## References

| dialect | reference image | identifies as |
|---|---|---|
| `scylla` | `scylladb/scylla:2026.3.3` (`--smp 1 --memory 1G --developer-mode 1 --overprovisioned 1`) | ScyllaDB 2026.3.3, `release_version` 3.0.8 |
| `cassandra` | `cassandra:5.0.9` (`MAX_HEAP_SIZE=1G HEAP_NEWSIZE=200M`) | Apache Cassandra 5.0.9 |

When a reference version changes, bump it here, in `crates/nosql-server/tests/common`, and in
the CI workflow, all in the same commit.

## What "1:1" covers

Everything a client can observe:

- **Native protocol.** Versions (ScyllaDB v3/v4; Cassandra v3/v4/v5 plus the v6-beta handling
  of 5.0), the version-negotiation errors, `SUPPORTED` options, compression (lz4 and snappy;
  v5 segment framing with CRC24/CRC32), and every opcode and flag. That includes keyspace per
  query, now-in-seconds, result metadata ids, warnings, custom payloads, tracing ids, events,
  and error codes with their bodies.
- **CQL.** Every statement in both dialects, with the same results, column metadata, row order,
  paging behaviour, warnings, and error codes and messages.
- **Types.** Every native type, collections (frozen and not), UDTs, tuples, `vector`,
  `duration` and `counter`. Literals, text and JSON formats, and casts all match.
- **System tables.** Exact columns, types and values for everything drivers and tools read:
  `system.local`, `system.peers(_v2)`, all of `system_schema`, `size_estimates`,
  `table_estimates`, `system_auth`, `system_traces`, `system_views`, ScyllaDB's `system.*`
  extras and `system_distributed`.
- **Driver extensions.** ScyllaDB shard awareness, LWT metadata marking, rate-limit errors and
  tablet routing.
- **Derived data.** Materialized views, secondary indexes, SAI, vector search, CDC and
  Alternator Streams, including their limitations and result ordering.
- **Auth.** Roles, permissions, data masking and service levels.
- **Server-side `DESCRIBE`**, byte for byte, per dialect.
- **Alternator** (the DynamoDB API) as ScyllaDB implements it. Where ScyllaDB itself differs
  from DynamoDB, we follow ScyllaDB.
- **ScyllaDB's REST API**, for the subset `nodetool` uses.

## How it is verified

Tests run in five layers, all in `pnpm test:nosql` (`scripts/test-nosql.sh`):

1. **Unit tests** in every crate.
2. **The differential harness** (`crates/nosql-server/tests/compat.rs`).
   - It runs scripts in `tests/compat/*.cql` against roll and against the reference image of
     each dialect.
   - It compares raw protocol results: column specs, rows, paging state presence, warnings,
     and error code and message.
   - Script directives handle order-insensitive results, volatile values (timeuuids, `now()`,
     host ids) and dialect-specific expectations.
   - It is the same pattern as `redis-server/tests/compat.rs`.
3. **Drivers.** Each driver runs the same suite against roll, in Docker.
   - **What the suite checks.** Connecting, plain and with PasswordAuthenticator, including a
     wrong password. Metadata as the driver parses it from `system_schema`: keyspaces, tables,
     UDTs, indexes, materialized views, functions and aggregates. Schema agreement, prepared
     statements with positional and named markers, paging and its resumption, logged, unlogged
     and counter batches, and LWT, including ScyllaDB's LWT marking. A round trip of every type:
     29 checks covering every native type, collections, nested frozen types, tuples, UDTs
     (frozen and not) and `vector`. Token-aware routing, where the driver's Murmur3 token must
     equal the server's `token()`. Shard-aware routing in the ScyllaDB forks, run against two
     shards. Tracing, schema events after DDL from another session, and reconnecting after the
     server restarts.
   - **Where it runs.** `tests/drivers.rs` (`docker_drivers`) runs it against the `roll/nosql`
     image in four configurations, ScyllaDB and Cassandra dialects each plain and with auth,
     UDFs and views. It runs the same suite against the matching reference container. A check
     that passes on the reference must pass on roll.
   - **Status:** roll passes every check that the reference passes, for every driver and every
     configuration that has a running reference.
   - The drivers:
     - Rust: the `scylla` crate, run in-process
     - Python: `cassandra-driver` 3.30.1 and `scylla-driver` 3.29.12
     - Java: `org.apache.cassandra:java-driver-core` 4.19.4 and `com.scylladb:java-driver-core`
       4.19.2.3
     - Go: `github.com/gocql/gocql` 1.7.0 and `github.com/scylladb/gocql` 1.20.0
     - Node: `cassandra-driver` 4.10.0
     - The AWS SDKs for Alternator
   - **Where the references fail.** These checks fail on the reference too, and roll reproduces
     each of them:
     - Against ScyllaDB, the DataStax Java, both Go and the Node drivers read a `DESC`
       clustering column as ascending, because ScyllaDB writes `clustering_order` in capitals.
     - gocql 1.7 cannot bind a tuple marker.
     - ScyllaDB's gocql fork finds no materialized views in its metadata.
     - Neither Go driver sees UDF metadata on ScyllaDB.
     - gocql 1.7 panics on an aggregate that has no final function.
4. **ScyllaDB's conformance suites.** These are `test/cqlpy` (about 2,100 tests, including
   Cassandra's own unit tests ported to Python) and `test/alternator` (about 1,600 tests). They
   are written to run against any CQL server and against DynamoDB.
   - `scripts/nosql-conformance.sh` downloads them at a pinned commit into `target/`. They are
     never committed, because they are under ScyllaDB's source-available licence.
   - It runs them in a Python container against roll in each dialect and against the reference
     image.
   - A test that passes on the reference must pass on roll. The committed files list test
     ids only, never test content: `crates/nosql-server/tests/conformance/reference-*.txt`
     record each reference's results, and `roll-known-failures-<dialect>.txt` lists every
     reference-passing test roll does not pass yet, with a tag (see
     [conformance-triage.md](conformance-triage.md)). The known-failures lists must only
     shrink.
5. **Tools.** Each tool must work unchanged against roll. `tests/tools.rs` runs them against
   the `roll/nosql` image, and `tests/image.rs` covers the image itself (see
   [local-dev.md](local-dev.md)):
   - Testcontainers' Cassandra and ScyllaDB modules, with the roll image swapped in, for:
     - Java 2.0.5, including an init script, `withConfigurationOverride` with
       PasswordAuthenticator, and Alternator
     - Python 4.15.0
     - Go 0.44.0, including init scripts, shard awareness and Alternator
     - Node 12.2.1
   - `golang-migrate` 4.18.3 (cassandra): up, version and down, in both dialects
   - `cassandra-stress` from `cassandra:5.0.9` (write and read) and `scylla-bench` 1.3.3
     (sequential write and uniform read, shard-aware)
   - the `cqlsh` of both reference images: DESCRIBE, SELECT and a COPY TO / FROM round trip.
     The output must be identical against roll and against the reference.
   - Not covered yet: `cassandra-migrate`, `cqlmigrate`, `latte` and Spark's connector
     metadata queries.

## Out of scope

These are differences we accept on purpose.

- **Cluster internals clients cannot reach.** This covers gossip, inter-node messaging, repair,
  hints, streaming between nodes, and mixed clusters with real Cassandra or ScyllaDB nodes. roll
  is never a node in someone else's ring. Use an import instead (see
  [preview-environments.md](preview-environments.md)).
- **JMX and Cassandra's Java `nodetool`.** ScyllaDB's REST `nodetool` works, and the CLI has
  equivalents.
- **On-disk SSTable formats.** We *read* Cassandra (`nb`, `oa`, `da`) and ScyllaDB (`me`, `ms`)
  SSTables for imports. We store our own format.
- **Java UDFs and Java triggers.** Both are disabled by default in the references; we return
  their errors. With UDFs enabled, Cassandra-dialect databases store Java functions and
  aggregates (DDL, `system_schema` rows, DESCRIBE and permissions match the reference) but don't
  compile or run them: a body that wouldn't compile is accepted, and a call that reaches a Java
  body fails with `FUNCTION_FAILURE` and the message
  `java.lang.UnsupportedOperationException: Java UDFs are not supported by roll` (the shape of
  Cassandra's own failures, which carry the exception's `toString()`).
- **ScyllaDB's Lua UDFs** run on Lua 5.4 with ScyllaDB's libraries, type mapping, limits and
  messages, with three differences: `dofile`/`loadfile` see no files and `print`/`warn` print
  nothing (ScyllaDB reads and writes the server's), `load` refuses binary chunks (Lua doesn't
  verify bytecode), and an aggregate's or a hook's error never crashes the database (ScyllaDB
  2026.3.3 segfaults on a non-string error object or on a non-table returned as a set, map or
  UDT; roll answers `lua execution failed: (error object is a table value)` and
  `value is not a table`).
- **ScyllaDB's WASM UDFs** (`LANGUAGE wasm`, WebAssembly text) run on the `wasmi` interpreter
  with ScyllaDB's ABI (versions 1 and 2, native and serialized arguments, `_scylla_malloc` /
  `_scylla_free`). A call is bounded by fuel (about 10^8 instructions) instead of ScyllaDB's
  time limit, and failures read `wasm UDF <name> failed: …` (the suite checks them by pattern:
  `type mismatch`, `Expected i32, got f32`, `fuel consumed`).
- **Unavailability and timeouts from replica failures.** These cannot happen in our model.
  Client-side timeouts still behave normally.
- **Timing-dependent details.** Exact compaction schedules, cache hit rates, and wall-clock
  values in tracing events.
- **Accord / multi-partition transactions** (Cassandra 6). We will add them when 6.0 is GA and
  a reference image exists.

## Status

The conformance pass rate per suite and dialect is tracked in [plan.md](plan.md).
