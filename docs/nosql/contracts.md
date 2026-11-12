# roll nosql — internal contracts

Every nosql crate follows these conventions. Read `AGENTS.md` and
[architecture.md](architecture.md) first.

## Ports

| listener | rolld default | flag / env | Docker image / standard |
|---|---|---|---|
| CQL (plain locally, TLS + SNI hosted) | `127.0.0.1:59042` | `--nosql-cql` / `ROLL_NOSQL_CQL` | 9042 |
| CQL, ScyllaDB shard-aware | `127.0.0.1:59043` | `--nosql-cql-shard-aware` / `ROLL_NOSQL_CQL_SHARD_AWARE` | 19042 |
| Alternator (DynamoDB API) | `127.0.0.1:58000` | `--nosql-alternator` / `ROLL_NOSQL_ALTERNATOR` | 8000 |
| Alternator over HTTPS (optional) | off | `--nosql-alternator-https` / `ROLL_NOSQL_ALTERNATOR_HTTPS` | `alternator_https_port` |
| ScyllaDB REST API | `127.0.0.1:59100` | `--nosql-rest` / `ROLL_NOSQL_REST` | 10000 |
| roll API (`/v1/nosql/...`) | rolld API `:7070` | | |

Other environment variables:

- `ROLL_NOSQL_TLS_CERT` / `ROLL_NOSQL_TLS_KEY` turn on TLS on the CQL ports.
- `ROLL_NOSQL_PROXY_PROTOCOL` accepts PROXY v1/v2 headers.
- `ROLL_NOSQL_DOMAIN` is the SNI suffix.
- `ROLL_NOSQL_SINGLE=<db>` runs single mode.
- `ROLL_NOSQL_ALTERNATOR_DOMAIN` is the Alternator host suffix (default `alternator.<base>` for a
  `nosql.<base>` domain); `ROLL_NOSQL_ALTERNATOR_TTL_PERIOD` is the TTL scan period in seconds
  (default 86400, as ScyllaDB's `alternator_ttl_period_in_seconds`).
- Alternator requests name their database by the SigV4 access key `role@<id>` or the host
  `<id>.alternator.<domain>`; the secret access key is the role's salted hash
  (`GET /v1/nosql/databases/{id}/alternator`).

## Identifiers

- **Database ids** are DNS labels: `n` followed by 11 lowercase base32 characters, for example
  `nk3v9q2m7xa1`. Branches are databases and get their own ids.
- **Names** are unique, lowercase `[a-z0-9-]{1,40}`.
- **Hosted endpoints** are `<id>.nosql.<domain>` for CQL and `<id>.alternator.<domain>` for
  Alternator.
- **The `host_id`** of the virtual node is a UUIDv5 of the database id. `cluster_name` defaults
  to the database name.
- **SSTable ids** are a timeuuid with a random suffix. They are never reused.

## Crate boundaries

The lower crates do not know about the higher ones. `nosql-proto`, `nosql-cql` and
`nosql-storage` do not depend on each other or on `nosql-types`, so they can be developed
independently. The engine joins them.

### `nosql-types`

- `CqlType` is the resolved type:
  - every native type
  - `List`, `Set` and `Map`, each with a `frozen` flag
  - `Tuple`
  - `Udt(Arc<UdtDef>, frozen)`
  - `Vector { elem, dim }`
  - `Custom(class name)`
- Values are kept as **serialised bytes in the native protocol format**, which is also the
  storage format. `Value` is the decoded form, used for literals, functions and formatting.
- The crate also provides:
  - `compare(ty, a, b)` with each type's server-side ordering (not byte order)
  - `validate(ty, bytes)`, which rejects malformed values with the reference's messages
  - CQL literal and cqlsh-style formatting
  - JSON conversion, as `SELECT JSON` and `INSERT JSON` use it
  - type-name parsing and printing (`frozen<map<text, int>>` and marshal class names)
  - `token(partition_key_bytes) -> i64`, Cassandra's Murmur3 including its sign quirk
  - composite partition-key encoding
  - timeuuid generation and the `now()` / `minTimeuuid` / `maxTimeuuid` helpers

### `nosql-proto`

- Frames for versions 3, 4 and 5:
  - the header, flags and stream id
  - v5's outer segment framing (self-contained and multi-segment frames, CRC24 header, CRC32
    payload, optional lz4)
  - per-frame body compression (lz4, snappy) for v3/v4
- One Rust type per request and response body. `Request::decode(version, opcode, body)` and
  `Response::encode(version, …)` gate every field on the protocol version.
- Protocol-level column types (`ColType`: option ids, custom class strings, nested
  collections, UDT and tuple specs) are defined here, independently of `nosql-types`. The
  engine and server convert between them.
- **Bounded decoding.** No allocation is sized from a peer-supplied count beyond the bytes
  actually present. Frame bodies are capped (256 MiB, as in Cassandra's
  `native_transport_max_frame_size`).
- `Codec` implements `tokio_util::codec::{Decoder, Encoder}` for frames, with v5 segment
  switching after `READY` / `AUTH_SUCCESS`.

### `nosql-cql`

- `parse(text) -> Result<Vec<Statement>, SyntaxError>` covers every statement of both dialects.
- The AST keeps literals as written. Types and terms are resolved later, by the engine.
- `SyntaxError` carries the line, column and the ANTLR-style message the reference prints, for
  example `line 1:7 no viable alternative at input 'FRM'`, so error texts can match.
- Bind markers (`?` and `:name`) are numbered in source order, as the reference does.

### `nosql-storage`

- Storage is independent of CQL. The engine supplies:
  - a `ClusteringComparator` (compares clustering prefixes and bounds)
  - the token of each partition key
- The data model is Cassandra's: `PartitionUpdate`, `Row`, `Cell`, `ComplexColumn`,
  `DeletionTime`, `LivenessInfo`, `RangeTombstone` (see architecture.md). Column ids are dense
  `u32`s assigned by the engine.
- `Table` operations:
  - `apply(update)`
  - `read(PartitionRead)` / `scan(TokenRange, …)`, returning merged, reconciled partitions as
    an async stream
  - `flush()`, `compact()`, `truncate()`, `stats()`
- `Store` handles persistence: the shipper (commit log to object storage), manifests,
  `open`/`unload`, `branch`, `restore_at`, and garbage collection. It uses
  `pg_storage::Objects`, and tests run on `Objects` with a temp directory, or S3 when
  `ROLL_E2E_S3_BUCKET` is set (like `redis_server::persist::test_objects`).

### `nosql-engine` (the facade the server uses)

```rust
pub struct Database;        // one tenant: schema, tables, auth, settings, dialect
impl Database {
    pub async fn open(cfg: DbConfig, store: StoreHandle) -> anyhow::Result<Arc<Database>>;
    pub fn connect(self: &Arc<Self>, client: ClientInfo) -> Session; // per connection
    pub fn dialect(&self) -> Dialect;
    pub fn events(&self) -> broadcast::Receiver<Event>;            // schema/topology/status
    pub async fn flush(&self) -> anyhow::Result<()>;
}
impl Session {
    pub fn needs_auth(&self) -> bool;
    pub async fn authenticate(&mut self, sasl: &[u8]) -> Result<AuthStep, Error>;
    pub async fn query(&mut self, text: &str, p: QueryParams) -> Result<Response, Error>;
    pub async fn prepare(&mut self, text: &str, keyspace: Option<&str>) -> Result<Prepared, Error>;
    pub async fn execute(&mut self, id: &[u8], p: QueryParams) -> Result<Response, Error>;
    pub async fn batch(&mut self, b: Batch) -> Result<Response, Error>;
}
```

- `Response` is one of `Void`, `Rows`, `SetKeyspace` or `SchemaChange`. It carries
  `warnings`, a tracing id and a custom payload.
- `Rows` carries the metadata (column specs with `CqlType`), the rows as serialised cells, and
  the paging state.
- `Error` maps one-to-one onto native protocol errors: the code, the message, and every
  code-specific field (`AlreadyExists { keyspace, table }`, `Unprepared { id }`, `ReadTimeout`,
  …).
- Prepared statements are shared per database, keyed by the MD5 of the keyspace and query text
  (as Cassandra does), so they survive reconnects.

### `nosql-server`

It follows `redis-server`:

- `Node::new(catalog, store, NodeConfig)`, where `Catalog::lookup(db) -> Option<DbSpec>`
- `Directory` resolves a connection to a database
- `net::serve(listener, dir, ListenOptions { tls, proxy_protocol, shard_aware })`
- `http::router(dir)` serves the dashboard data API: CQL over JSON, the schema, and table pages
- `alternator::serve` and `rest::router`
- `Single` serves single mode

## Errors and limits

- **No panic is reachable from a peer.** This covers malformed frames, values, CQL and
  DynamoDB JSON. Index and slice operations on peer data must be checked.
- **Limits** default to the reference's. These include:
  - frame size
  - collection size (64K elements in v3+)
  - partition key size (65535 bytes)
  - clustering key size
  - batch size warn/fail
  - tombstone warn/fail
  - page size
- **Allocations.** Integration tests run under the capped allocator from
  `crates/stream-broker/tests/common` (any single allocation over 1 GiB fails). Copy it into
  `crates/nosql-server/tests/common`.

## Tests

- Unit tests live in each crate.
- Integration tests live in `crates/nosql-server/tests/`:
  - `compat.rs` + `compat/*.cql`: differential against the reference images
  - `drivers.rs`: the Rust `scylla` crate, plus the other drivers in Docker
  - `conformance.rs`: runs ScyllaDB's `test/cqlpy` and `test/alternator` (via
    `scripts/nosql-conformance.sh`), checked against `conformance/reference-*.txt` and
    `conformance/roll-known-failures-<dialect>.txt`
  - `storage.rs`, `branches.rs`, `restore.rs`: persistence, against S3 when
    `ROLL_E2E_S3_BUCKET` is set
- `pnpm test:nosql` (`scripts/test-nosql.sh`) runs all of it and skips itself when nothing
  relevant changed, like `test:redis`.
- Reference containers use the images in [compatibility.md](compatibility.md). Tests reuse a
  running reference when one is given (`ROLL_NOSQL_REF_SCYLLA=127.0.0.1:19042`,
  `ROLL_NOSQL_REF_CASSANDRA=127.0.0.1:29042`). Otherwise they start one with `--rm` and remove
  it in `Drop`. Run only one reference container at a time: the dev machine has little RAM.

## Building (agents)

The machine has about 30 GB of RAM, shared with other sessions. Memory exhaustion has
frozen it before, so:

- Run cargo only through `~/.cache/roll-nosql/cargo.sh <args> -p <crate>`. It waits for 6 GB
  of free RAM, takes a machine-wide build lock, and sets `CARGO_TARGET_DIR=target/agents/shared`
  and `CARGO_BUILD_JOBS=4`.
- Build with `-p <crate>`, never the whole workspace.
- Never start reference containers. Use the shared ones the lead runs:
  - `nosql-ref-scylla`: 127.0.0.1:19042 (CQL), :18000 (Alternator), :10000 (REST)
  - `nosql-ref-cassandra`: 127.0.0.1:29042
  - `nosql-ref-scylla-auth` (127.0.0.1:19043) and `nosql-ref-cassandra-auth` (127.0.0.1:29043):
    feature-enabled references. Both use PasswordAuthenticator and CassandraAuthorizer with
    superuser `cassandra`/`cassandra`. ScyllaDB also has Lua UDFs (`experimental-features=udf`).
    Cassandra also has UDFs, materialized views, SASI and dynamic data masking.
  - Use your own keyspace prefix and drop it when done.
- Reference sources are in `~/.cache/roll-nosql/` (`cassandra-src`, and `scylla-src` for docs
  and tests only).
- Don't commit. The lead commits.

## roll API (`/v1/nosql`)

```
GET    /v1/nosql                                  info: version, endpoints, dialects, defaults, pricing, memory (budgets and use)
GET    /v1/nosql/databases                        list
POST   /v1/nosql/databases                        {name, dialect?, datacenter?, rack?, cluster_name?, settings?}
GET    /v1/nosql/databases/{id}                   detail: endpoints, credentials, state, parent, settings
PATCH  /v1/nosql/databases/{id}                   settings, idle_timeout, durability, history_retention, masking, read_only, expires_at
DELETE /v1/nosql/databases/{id}
POST   /v1/nosql/databases/{id}/branches          {name, at?, schema_only?, expires_at?, read_only?}
POST   /v1/nosql/databases/{id}/reset             {at?}   re-branch from parent, keep id/endpoint/credentials
POST   /v1/nosql/databases/{id}/restore           {at}    in place; the replaced state is archived
GET    /v1/nosql/databases/{id}/diff              schema diff against the parent (CQL)
POST   /v1/nosql/databases/{id}/imports           {from: "cql://…" | "s3://…" | "file:///…", keyspaces?, every?}
GET    /v1/nosql/databases/{id}/imports
POST   /v1/nosql/databases/{id}/password          rotate the initial superuser's password
POST   /v1/nosql/databases/{id}/cql               {query, keyspace?, page_state?, page_size?, trace?}  console
GET    /v1/nosql/databases/{id}/schema            keyspaces, tables, types, views, indexes, DESCRIBE text
GET    /v1/nosql/databases/{id}/usage?days=       metering, cost and the memtable cap
GET    /v1/nosql/databases/{id}/alternator        Alternator endpoint, access key id and secret (the salted hash)
GET    /v1/nosql/databases/{id}/activity          clients, slow queries, warnings, memory (memtables, cap, cached blocks)
POST   /v1/nosql/databases/{id}/flush | /compact | /unload
```

Errors are `{"error": "..."}` with 400, 404, 409 or 500, as in `redis-control`.
