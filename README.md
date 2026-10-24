# roll

Serverless infrastructure for everything.

## Layout

```
apps/
  dashboard/        Next.js 16 app — brand playground (/), dashboard (/dashboard, /dashboard/databases, /dashboard/streams, /dashboard/tasks, /dashboard/storage, /dashboard/nosql)
packages/
  brand/            @roll/brand — logo geometry, animation engine, React components, SVG assets
  tasks/            @roll/tasks — TypeScript SDK: client, pull consumer, signature verification, workflows
crates/
  pg-proxy/         Postgres wire-protocol proxy (routing, wake-on-connect, cancel forwarding)
  pg-control/       Control plane: projects, branches, endpoints, autoscaling, metering, HTTP API
  redis-proto/      RESP2/RESP3 parser and encoder
  redis-engine/     Redis data structures and commands (pure, synchronous, one engine per shard)
  redis-server/     Redis network layer: RESP + TLS, REST API, tenants, durability on S3, replicas
  redis-control/    Redis control plane: databases, branches, restore, replicas, metering, HTTP API
  nosql-types/      CQL data types: values, ordering, serialisation, literals, JSON, Murmur3 tokens
  nosql-proto/      CQL native protocol v3/v4/v5 (both directions) and an async client
  nosql-cql/        CQL lexer and parser for Cassandra 5 and ScyllaDB, with the references' syntax errors
  nosql-storage/    LSM storage on object storage: memtables, SSTables, compaction, commit log, branches, PITR
  nosql-engine/     CQL engine: schema, queries, LWT, indexes, views, CDC, auth, UDFs (Lua 5.4), DESCRIBE, dialects
  nosql-lua/        Vendored Lua 5.4 for ScyllaDB Lua UDFs (symbols prefixed so it links next to redis-lua)
  nosql-alternator/ ScyllaDB Alternator: the DynamoDB API on top of the engine
  nosql-server/     NoSQL data plane: CQL (+TLS, SNI, shard-aware), Alternator, ScyllaDB REST, tenants, scale to zero
  nosql-control/    NoSQL control plane: databases, branches, restore, imports, masking, metering, HTTP API
  stream-log/       Kafka-format partition log: segments, group commit, idempotence, transactions
  stream-broker/    Kafka-compatible broker: wire protocol, coordinators, tiering, compaction, HTTP API
  tasks/            roll tasks: messages, queues, schedules, workflows (SQLite, its own S3 log store, or DynamoDB)
  s3-proto/         S3 wire protocol: SigV4 (headers, presigned, aws-chunked + trailers), XML, checksums, errors
  s3-store/         roll storage metadata: SQLite + durable log on S3, branching, refcounts, trash, GC, blob backend
  s3-server/        S3-compatible HTTP API: routing, auth, handlers, streaming to and from S3
  s3-control/       roll storage control plane: buckets, branches, keys, browsing, usage and pricing (/v1/storage)
  rolld/            Daemon: proxy + API + reconciler + Redis + NoSQL + Kafka listener + S3 API (+ roll tasks on /v1/tasks)
  roll-cli/         `roll` CLI
deploy/aws/         roll tasks on AWS: Dockerfile + CloudFormation
```

## Develop

```sh
pnpm install
pnpm dev            # http://localhost:3000
pnpm typecheck
pnpm build
```

## The marks

- **Logo — Horizon** (`HorizonMark`, `RollLogo`): a banded half-globe rising over a line. Used in the
  header, sidebar, favicon and profile picture. Solid at rest; at ≥56px it dissolves into nodes when the
  cursor comes close or the product is busy. Below 56px it stays solid and states play as light through
  the bands, so it is always recognisable.
- **Globe** (`RollMark`): a globe of nodes (one blue accent node = "your app, live"). The large interactive
  hero piece and status visual. Both marks animate the same six states:

| state       | motion                                                     |
| ----------- | ---------------------------------------------------------- |
| `idle`      | nodes breathe, accent node lit                             |
| `thinking`  | meridians sweep the globe (AI reasoning)                   |
| `deploying` | globe settles into the horizon formation and rises         |
| `success`   | ripple from the core                                       |
| `error`     | nodes jolt and cool to red                                 |
| `sleep`     | scaled to zero — dim horizon                               |

```tsx
import { RollMark, RollLogo, DotField } from "@roll/brand";

<RollMark state="thinking" size={120} />      // canvas mark; hover attracts, click ripples
<RollLogo state={state} size={20} />          // Horizon logo + "roll" wordmark
<HorizonMark state="deploying" size={64} />   // logo alone; dissolves into nodes on hover
<DotField mode="thinking" />                  // interactive node background, fills its parent
```

- `RollMarkEngine` (in `engine.ts`) is framework-free; React is a thin wrapper.
- Colour follows CSS `color` (`currentColor`), so it works on light and dark.
- `prefers-reduced-motion` renders static frames.
- In the dashboard, `MarkStateProvider` shares one state so a deploy or an AI question animates every mark.

## Assets

`pnpm --filter @roll/brand export` regenerates static SVGs into `packages/brand/assets/`,
`apps/dashboard/public/brand/`, and the favicon `apps/dashboard/src/app/icon.svg` — from the same
geometry the animation uses.

## AI assistant

`apps/dashboard/src/lib/ask.ts` is a simulated assistant (thinking steps + streamed answer).
Replace `answerFor` with a real model call; the UI only depends on the `{ steps, answer }` shape.

## Postgres

Serverless Postgres: real, unmodified Postgres (16, 17 or 18) as the compute engine, so every query,
extension, driver and ORM works. The serverless layer around it is written in Rust.

```sh
cargo run -p rolld                      # API on :7070, Postgres proxy on :55432 (needs Docker)
cargo run -p roll-cli -- db create shop # prints a connection string
psql "$(cargo run -q -p roll-cli -- db url shop)"
cargo run -p roll-cli -- db branch shop dev
cargo run -p roll-cli -- db usage shop
```

The dashboard at `/dashboard/databases` shows databases, branches, live compute size, a SQL console,
autoscaling settings, usage and the AWS cost estimate.

How it works:

- **Proxy.** Clients connect to the proxy, which reads the startup packet, finds the endpoint
  (`?options=endpoint%3D<id>`) and wakes its compute. It then splices bytes, so authentication stays
  end to end. Cancel requests (Ctrl-C) are routed to the right compute.
- **Scale to zero.** A compute stops after `suspend_timeout_s` with no connections. The next connection
  starts it again, which takes about 0.5s locally.
- **Autoscaling.** Computes are resized live between `min_cu` and `max_cu` from CPU load
  (0.25 → 0.5 → 1 → 2 … CU) with no restart and no dropped connections.
- **Branches.**
  - From a suspended parent: a copy-on-write clone (APFS / reflink), about 200 ms.
  - From a running parent: `pg_basebackup`, about 1s, with no downtime on the parent.
- **Metering.** Compute is metered in CU-seconds (segments that change at every resize), and storage is
  sampled per branch. Costs are priced at AWS Graviton on-demand (1 CU = 1 vCPU + 4 GiB at $0.0408/h)
  and gp3 storage.

Measured locally on an M-series Mac with Docker Desktop:

| what | result |
| --- | --- |
| create database (initdb + boot) | 3.8 s |
| cold start from zero, connect + query | 0.66 s end to end |
| branch, parent suspended (CoW) | 205 ms |
| branch, parent running (basebackup) | 967 ms |
| pgbench `-S`, 8 clients, through proxy | 10.3k TPS while autoscaling 0.25 → 1 CU |

### Tests

`pnpm test:pg` (or `scripts/test-postgres.sh`) runs the unit tests, clippy and an end-to-end suite
against real Postgres 16/17/18 computes in Docker. The suite covers the wire protocol and proxy errors,
scale to zero, live resize, branching, PITR, restore in place, eviction, disk loss, replicas, retention,
metering and the HTTP API. It only runs when a Postgres crate, `Cargo.toml`/`Cargo.lock`, the script
or the workflow changed since the last green run. Pass `--force` to run anyway, or `--unit` to skip Docker.
CI (`.github/workflows/postgres.yml`) is path-filtered the same way. It also repeats the storage tests
against S3 (SeaweedFS). To point the e2e tests at S3 locally, set `ROLL_E2E_S3_BUCKET` and
`ROLL_E2E_S3_ENDPOINT`.

### Compared with Neon and PlanetScale

| feature | status |
| --- | --- |
| Full Postgres compatibility (16/17/18, extensions, `pg_stat_statements`) | ✅ |
| Scale to zero, wake on connect | ✅ |
| Live autoscaling (CPU and memory, no restart) | ✅ |
| Branching (instant CoW / online) | ✅ (full copies; not yet page-level shared storage) |
| Usage metering and cost estimate | ✅ |
| SQL console, API, CLI, dashboard | ✅ |
| TLS + SNI routing (`ep-xxx.region.roll.sh`) | next |
| Connection pooling (transaction mode) | next |
| Bottomless storage: WAL safekeeper + base images on S3, idle branches evicted | ✅ (no pageserver yet: a cold branch restores whole) |
| Point-in-time restore, branch from any timestamp or LSN, restore in place | ✅ |
| Read replicas (keep serving while the primary sleeps) | ✅ |
| AWS deployment (EC2/ECS Graviton, EBS → S3) | next |
| Schema-change workflows: deploy requests, safe migrations, revert | later (PlanetScale) |
| Horizontal sharding (Vitess-style) | later |
| Query insights | later (`pg_stat_statements` is already loaded) |

"Faster and cheaper than regular Postgres" comes from several places: computes run only while used,
they are sized to the load, they run on Graviton, the settings are tuned (`jit=off`, SSD costs, WAL
compression), and pooling comes next. The query engine itself is stock Postgres; that is what keeps it
100% compatible.

## Redis

Serverless Redis, written in Rust and compatible with Redis 8 clients. It speaks RESP2/RESP3 over TCP
or TLS, and also has an Upstash-compatible REST API, so serverless and edge functions need no connection.
Data is durable in object storage (S3), so a database that nobody uses costs nothing but storage.

```sh
cargo run -p rolld                                # RESP on :56380, REST on :58080
cargo run -p roll-cli -- redis create cache --memory 256
redis-cli -u "$(cargo run -q -p roll-cli -- redis url cache)"
cargo run -p roll-cli -- redis rest cache         # UPSTASH_REDIS_REST_URL / _TOKEN
cargo run -p roll-cli -- redis branch cache dev --at 15m
cargo run -p roll-cli -- redis usage cache        # cost, next to ElastiCache Serverless
```

Any Redis client works with the `redis://` URL. The REST endpoint works with `@upstash/redis` or plain
`curl` (`curl $URL/incr/n -H "Authorization: Bearer $TOKEN"`).

How it works:

- **Many tenants per node.** One process serves many databases. Clients pick a database in one of
  three ways:
  - the TLS server name (`<db>.<domain>`);
  - `AUTH user@<db> <password>`;
  - a single `<db>_<secret>` token.
- **Scale to zero.** A database is loaded on its first `AUTH` or HTTP request. It is unloaded after
  `idle_timeout_s` with no clients, and after that it costs nothing but S3.
- **Bottomless durability.**
  - Every write becomes a deterministic effect. Effects are written in Redis's replication format:
    `EXPIRE` becomes `PEXPIREAT`, `SPOP` becomes `SREM`, and so on.
  - Each node group-commits the effects of all its tenants into one zstd segment per second. S3 PUTs
    therefore scale with the number of nodes, not tenants.
  - `durability: sync` makes replies wait until the write is in S3. `WAITAOF` reports the same thing.
- **Point in time.**
  - Branches are zero-copy: a branch's log starts where the parent's left off.
  - You can branch from any second in the retention window.
  - Restore in place keeps the replaced state as an archive.
- **Read replicas.**
  - A replica streams from the primary's backlog, and `WAIT` counts replica acks.
  - When the primary is asleep, a replica tails S3 instead.
  - Each replica gets its own read-only endpoint, and replicas can be promoted.
- **AWS-friendly networking.**
  - TLS with SNI routing.
  - PROXY protocol v1/v2, for client IPs behind an NLB.
  - Plain S3, or any S3-compatible store, for storage.
- **Metering.** The control plane meters GiB-hours while a database is loaded, plus commands, egress and
  storage. Costs are priced at Graviton memory ($0.0067/GiB-h) and compared with ElastiCache Serverless
  ($0.125/GB-h, with a 1 GB minimum).

Commands:

- **Data types:**
  - strings, keys and expiry;
  - lists;
  - hashes, with field TTLs;
  - sets and sorted sets;
  - geo;
  - streams, with consumer groups;
  - HyperLogLog and bitmaps.
- **Transactions:** MULTI/EXEC and `WATCH`.
- **Pub/sub,** including sharded pub/sub.
- **Server:** ACLs, `CLIENT`, `CONFIG`, `INFO` and `SCAN`.
- **Behaviour:** blocking ops, eviction policies and keyspace notifications.

Still in progress: Lua scripting and functions, `DUMP`/`RESTORE` and RDB import/export, JSON, and the Bloom
family.

Storage stays bounded:

- A database snapshots once its log grows larger than its data, and again when it unloads.
- Snapshots and log older than `history_retention_s` are dropped.
- A garbage collector deletes objects that no database, branch or restore archive references any
  more. An object must be found unreferenced on two passes an hour apart before it is deleted.
- A branch records the part of its parent's history it starts from, so the parent can be deleted
  without breaking the branch.

### Tests

`pnpm test:redis` (or `scripts/test-redis.sh`) runs unit tests, clippy, and the integration suites:

- the REST API;
- durability and PITR;
- tenants, TLS and eviction;
- replication;
- the control plane.

It also runs a differential compatibility suite. That suite sends the same scripts to roll and to
`redis:8-alpine` in Docker and compares the replies byte for byte.

The script skips itself when nothing relevant changed. Pass `--force` to run anyway, or `--unit` to skip
Docker. CI is in `.github/workflows/redis.yml`. To run the durability tests against S3, set
`ROLL_E2E_S3_BUCKET` and `ROLL_E2E_S3_ENDPOINT` (plus AWS credentials).

## NoSQL

A ScyllaDB- and Cassandra-compatible wide-column database, written in Rust. Each database has a
dialect, `scylla` (ScyllaDB 2026.3.3) or `cassandra` (Cassandra 5.0.9), and behaves like that server
for every client: drivers, cqlsh, ScyllaDB's `nodetool` over its REST API, and the AWS SDKs through
Alternator. Data lives in object storage, a database nobody uses costs only its S3 bytes, and a
branch of any size is ready instantly. The main use is a copy of a production cluster per pull
request, developer or CI run (`docs/nosql/preview-environments.md`).

```sh
cargo run -p rolld                                   # CQL on :59042, Alternator on :58000, REST on :59100
cargo run -p roll-cli -- nosql create app --dialect scylla --datacenter us-east-1
cargo run -p roll-cli -- nosql cqlsh app             # built-in cqlsh, same output as the real one
cargo run -p roll-cli -- nosql import app --from cql://reader:pw@prod:9042 --keyspaces shop
cargo run -p roll-cli -- nosql branch app --name pr-482 --expires 72h
cargo run -p roll-cli -- nosql diff pr-482           # schema change against the parent, as CQL
cargo run -p roll-cli -- nosql connect pr-482        # Java, Python, Go, Rust and Node snippets
```

How it works (details in `docs/nosql/`):

- **The cluster a client sees.** A database presents one healthy node in a single datacenter, with
  the datacenter, rack and cluster names you choose, so drivers configured for production work
  unchanged. Every consistency level is satisfied, schema agreement is immediate, and ScyllaDB's
  shard-aware, LWT and tablet extensions are advertised as ScyllaDB does.
- **Many tenants per node.** Connections pick a database by TLS server name (`<db>.<domain>`) or by
  the login `role@<db>`. A database loads on first use and unloads after its idle timeout.
- **Storage.** An LSM tree per table, with our own SSTable format built for range reads from S3, a
  memory and disk block cache, and every compaction strategy of both products (visible effects such
  as tombstone purging match). Each node ships one commit-log segment for all its tenants every
  ~100 ms; `durability: sync` acknowledges a write only once it is in S3.
- **Branches and point in time.** Branches reference their parent's SSTables, so creating one
  writes a single manifest. Reset keeps a branch's endpoint and credentials, restore runs in place,
  and branches can expire, be read-only, hold only the schema, or carry a masking policy that
  anonymises production data for every role.
- **Getting data in.** A live import from any Cassandra or ScyllaDB cluster over CQL keeps write
  times and TTLs, and can refresh on a schedule.
- **Metering.** Storage, requests and connection time, with a cost estimate next to Amazon
  Keyspaces, DynamoDB, Astra and ScyllaDB Cloud.

### Tests

`pnpm test:nosql` (or `scripts/test-nosql.sh`) runs clippy and every crate's tests. It then starts
`scylladb/scylla:2026.3.3` and `cassandra:5.0.9` (or reuses running ones given in
`ROLL_NOSQL_REF_*`) for the tests that compare roll with them:

- the differential scripts, which send the same CQL to roll and the reference and compare raw
  results;
- the Rust driver;
- cqlsh output;
- imports.

`--conformance` adds ScyllaDB's own `test/cqlpy` and `test/alternator` suites, run against roll.
`scripts/nosql-conformance.sh` downloads them at test time; they are never committed. Every test
the reference passes must pass on roll, apart from the lists in
`crates/nosql-server/tests/conformance/roll-known-failures-*.txt`, which may only shrink. Current
numbers are in `docs/nosql/plan.md`. CI is in `.github/workflows/nosql.yml`.

## Streams

Kafka-compatible streams: any Kafka client, tool or framework connects to `rolld` on `:9092`. You can
also skip the client entirely and produce and consume over plain HTTP. Storage is bottomless: data is
fsynced locally and uploaded to object storage (local directory or S3), and idle partitions close and
cost nothing but their objects.

```sh
cargo run -p rolld                                        # Kafka on :9092, HTTP API on :7070
cargo run -p roll-cli -- stream create shop               # bootstrap address, SASL key, client properties
cargo run -p roll-cli -- stream topic create shop orders -p 6
echo '{"id":1}' | cargo run -q -p roll-cli -- stream produce shop orders -k user-1
cargo run -p roll-cli -- stream consume shop orders --group billing --follow

curl localhost:7070/v1/streams/shop/topics/orders/records -d '{"key":"user-1","value":{"id":2}}'
curl 'localhost:7070/v1/streams/shop/topics/orders/records?from=earliest&wait_ms=10000'
curl -N localhost:7070/v1/streams/shop/topics/orders/tail     # Server-Sent Events
```

Unauthenticated Kafka connections use the `default` stream (`--default-stream`). Other streams
authenticate with SASL SCRAM-SHA-256/512 or PLAIN using a stream key, and each stream is its own
isolated cluster: topics, groups, transactions and quotas. `/dashboard/streams` shows throughput, topics,
partitions (hot vs object-storage bytes), keys, a live tail and the AWS cost next to MSK.

How it works:

- **Zero-copy data path.** Produce validates record batches (CRC, sequence, size, timestamps) and
  appends them as received, patching only the offset. Fetch returns the same bytes. A recent-batch
  cache serves tailing consumers from memory. Responses are pipelined in order, and long-polling
  fetches wake on the high watermark.
- **Durability.** `acks=all` waits for a group-commit fsync, and consumers only see durable data, so a
  crash never exposes records that could be lost. On restart, torn tails are cut at the last valid batch.
- **Tiering.** Sealed (and sufficiently old) data is uploaded as chunks of whole batches. Local copies
  are dropped by `local.retention.*`, and reads below the local range are served from object storage
  through a cache. `retention.*`, `DeleteRecords` and topic deletion remove the objects.
- **Compaction.** `cleanup.policy=compact` keeps the latest record per key. It drops keyless records,
  aborted transactional data and, after `delete.retention.ms`, tombstones. It rewrites uploaded chunks
  into new objects and swaps them in atomically; offsets never change. The trigger is
  `min.cleanable.dirty.ratio`, `min.compaction.lag.ms` or `max.compaction.lag.ms`.
- **Semantics.**
  - Idempotent producers (KIP-360 recovery).
  - Transactions with KIP-890 verification, the LSO and read_committed.
  - Exactly-once consume-transform-produce.
  - Classic consumer groups, including static membership (KIP-345).
  - Quotas with KIP-219 throttling.
- **HTTP data plane.**
  - Produce JSON, NDJSON or raw bodies. Partitioning uses Kafka's murmur2, so keys land where Kafka
    clients put them.
  - Read with stateless cursors, or through server-managed groups. The broker leases partitions per
    consumer and commits for you (`auto`, `read` or `manual`), and the committed offsets are the same
    ones Kafka groups see.
  - Confluent REST v3 (`/kafka/v3/clusters/...`) for existing tooling.

### Tests

`pnpm test:stream` (or `scripts/test-stream.sh`) runs clippy and the suite:

- `protocol.rs`: every API at every version (default and populated requests), version negotiation,
  pipelining, malformed frames, SASL and tenancy, quotas.
- `rdkafka.rs`: librdkafka for produce/consume across codecs, idempotence, groups, seeks and
  timestamps, transactions and EOS, admin, SASL, restarts.
- `java.rs`, against `apache/kafka:4.1.0` in Docker:
  - `kafka-topics`, the console clients, `kafka-consumer-groups`, `kafka-configs`
  - perf tools, `kafka-transactions` and the verifiable clients
  - `kafka-delete-records`, `kafka-log-dirs`, `kafka-metadata-quorum`
  - SASL SCRAM/PLAIN and TLS
  - a Kafka Streams word count with `exactly_once_v2`
- `storage.rs`: tiered reads from object storage, retention, torn writes, unclean stops, scale to zero,
  open transactions across restarts.
- `compaction.rs`: latest value per key, tombstones, librdkafka reading across gaps, compact+delete.
- `http.rs`: the control plane, produce formats, cursors, managed groups, SSE, auth, Confluent v3,
  and interop with Kafka clients.

The suite only runs when a stream crate changed since the last green run (`--force`, `--unit` as for
Postgres). CI (`.github/workflows/stream.yml`) also runs the storage, compaction, HTTP and librdkafka
tests against S3 (SeaweedFS).

Measured locally with a release build on an M-series Mac, using a single `rolld`. The Java perf tools
run in Docker Desktop, so their bytes also cross its VM network:

| what | result |
| --- | --- |
| librdkafka produce, 200 B records, `acks=all`, 6 partitions (`examples/bench.rs`) | 0.8–1.1M rec/s (160–225 MB/s) |
| librdkafka produce, 1 KB records, `acks=all` | 246k rec/s (252 MB/s) |
| librdkafka consume, 200 B / 1 KB records | 1.44M rec/s (289 MB/s) / 1.04M rec/s (1.07 GB/s) |
| `kafka-producer-perf-test`, 200 B, `acks=all`, uncompressed / lz4 (Docker) | 321k / 300k rec/s (61 / 57 MB/s: Docker Desktop's network is the limit) |
| `kafka-producer-perf-test` at 1,000 rec/s, `acks=all`, `linger.ms=0` (Docker) | p50 14 ms, p99 63 ms, fsync included |
| `kafka-consumer-perf-test` (Docker) | 509k rec/s (97 MB/s) |
| HTTP produce: 100k records (20 MB) in one NDJSON request | 158 ms until durable (630k rec/s) |
| `rolld` memory after 5M records | 243 MB RSS |

The data path doesn't decode records, so throughput follows bytes and batches, not record count.
[`crates/stream-broker/README.md`](crates/stream-broker/README.md) covers the internals, cold start,
idle usage, how to reproduce these numbers, and the current limits.

### Compared with Kafka, Confluent and Redpanda

| feature | status |
| --- | --- |
| Kafka protocol (produce v3–13, fetch v4–18, admin, groups, transactions, SASL, TLS, quotas) | ✅ |
| Java clients, librdkafka (Go/Python/.NET/Node), Kafka Streams, Kafka CLI tools | ✅ tested |
| Bottomless storage in S3, scale to zero, no disks to size | ✅ |
| Log compaction | ✅ |
| HTTP produce/consume without a client, SSE, Confluent REST v3 | ✅ |
| Multi-tenant streams with per-stream keys, quotas, usage and cost | ✅ |
| Online partition and config changes | ✅ |
| Replication across brokers / multi-AZ | next: durability today is local fsync plus object storage (seconds of RPO for the active segment on host loss) |
| Several brokers serving one cluster (partition leadership) | next: one `rolld` serves every partition today |
| KIP-848 consumer protocol, share groups (KIP-932) | next: classic groups only |
| Fetch sessions (KIP-227) | next: full fetches only (clients fall back automatically) |
| ACLs | next: keys scope a whole stream; `DescribeAcls` reports security disabled |
| Schema Registry, Connect | later |

## Tasks

roll tasks delivers HTTP messages and runs durable workflows: a Cloud Tasks / QStash / Inngest
alternative written in Rust. On AWS it runs on its own storage engine, the **log store**, with no
database service: SQLite on the leader node, made durable by shipping group commits to S3. A
Lambda function URL or a Network Load Balancer fronts a small Fargate cell (a leader and a hot
standby), which can host many tenants.

```sh
cargo run -p rolld                                          # serves /v1/tasks too (SQLite in .roll/)
cargo run -p tasks --bin roll-tasks                         # or standalone on :7071
roll tasks publish https://example.com/hook -b '{"hi":1}' --delay 30s --retries 5
roll tasks schedules create '0 9 * * MON-FRI' --url https://example.com/report --tz Europe/Budapest
roll tasks send user.signup '{"id":7}'
roll tasks messages · dlq · queues · topics · functions · runs · usage
```

The dashboard is at `/dashboard/tasks`, the TypeScript SDK is `@roll/tasks` (`packages/tasks`), and
the CLI is `roll tasks …` (`ROLL_TASKS_URL`, `ROLL_TASKS_TOKEN`).

What it does:

- **Messages.** Publish to a URL, a topic (fan-out) or a pull queue, from JSON or raw (`/publish/<url>`
  with `Roll-*` headers), singly or in batches.
  - Options: delay or `not_before` (up to a year), retries with exponential/linear/fixed backoff and
    jitter, `Retry-After` honoured, and per-attempt timeouts.
  - Status 489 or a `roll-non-retryable` header stops retries.
  - Success and failure callbacks.
  - Deduplication by id or content.
  - Labels.
  - A dead-letter queue with retry.
  - Full attempt history, including each response.
- **Signing.** Deliveries are signed per Standard Webhooks with two keys, so rotation never breaks
  receivers. The SDK verifies them.
- **Queues.**
  - Parallelism, GCRA rate limits, pause and resume, default retries and timeouts, and partitions
    for throughput.
  - Per-key flow control across messages.
  - FIFO groups: in order, one at a time, with retries holding the line.
  - Pull consumers: lease, ack, nack (or dead-letter), extend, long-poll.
- **Schedules.** Cron (5 or 6 fields, `@daily`, `L`, …) in any time zone, or `@every 5m`. A schedule
  publishes, sends an event or runs a function, fires exactly once per time across workers, and can be
  paused or triggered now.
- **Workflows.** Inngest-style durable functions served by your app over HTTP:
  - Steps: memoised `run` steps (with retries and their own backoff), `sleep`, `wait_for_event`
    (with match and `if` expressions, plus timeouts), `invoke` other functions, `send_event`, and
    parallel steps.
  - Triggers: events (with `if`), cron.
  - Flow and lifecycle: concurrency (keyed), throttle, rate limit, debounce, batching, idempotency,
    priority, `cancel_on`, start/finish timeouts, singleton, pause, cancel, rerun, wait for the result.
  - Lifecycle events: `roll/function.finished|failed|canceled`.
- **Ops.**
  - Auth: an admin token plus API keys.
  - SSRF protection: private and link-local destinations are blocked, and the check runs again on
    DNS resolution.
  - Usage metering with an AWS cost estimate.
  - Graceful drain on SIGTERM: in-flight deliveries finish or go back to the queue.
  - Workers share lanes by rendezvous hashing and take over a dead worker's lanes within seconds.
    Leases are extended for long deliveries.
  - Delivery is at-least-once.

### Storage: the log store

- **Writes.** Every write lands in SQLite on the leader. The net row changes (SQLite session
  changesets) go to S3 as one object per **group commit**: `tasks/wal/<seq>`.
  - API writes return once their commit is in S3, which is multi-AZ.
  - Worker bookkeeping (leases, delivery outcomes) rides along with the next commit, or with a lazy
    one within 5 seconds. Losing it in a crash only means a redelivery.
  - Recovered state is always an exact prefix of history.
- **Snapshots.** These are copied a few pages at a time while serving, then compressed and uploaded.
  Old log objects are deleted.
- **Recovery.** A node rebuilds from the newest snapshot plus the log after it.
- **Fencing.** Commits are create-only (`If-None-Match`). A new leader claims the next sequence
  number before serving, so a deposed leader can't commit and stops. Split brain can't lose
  acknowledged writes.
- **Failover.** Standbys health-check the leader directly. When it dies, one takes the lease (a
  compare-and-swap on `tasks/lease`) and serves within a few seconds. On SIGTERM (deploys, Spot
  reclaims) the leader drains, commits and hands over in about a second.
  - The front door (Lambda proxy, or any standby behind the load balancer) holds requests during
    the switch.
  - A request cut off mid-switch gets a retryable 503 that the proxy retries for you.
- **Tenants.** One cell hosts many tenants. Each has its own SQLite database, engine and API keys
  (`/t/<tenant>/v1/tasks`), and all of them share the cell's commits, nodes and connection pool.
  Managed with the node's token: `roll tasks tenants create|list|key|delete`.
- **Retention.** Keeping data longer costs S3 storage ($0.023/GB-month) and local disk, nothing per
  request. The default is 30 days.
- **Backups.** The bucket is versioned, and overwritten or deleted objects are kept for 30 days
  (`BackupDays`). `roll-tasks restore --at <time>` puts a stopped cell back to how it was at any
  second in that window, and can itself be undone. See
  [deploy/aws/README.md](deploy/aws/README.md#backups-and-point-in-time-restore).

### Cost

S3 bills per commit, not per message. A commit starts at most every `ROLL_TASKS_COMMIT_MS` (100 ms
by default) and carries every write made since the last one, from every tenant. So the storage
bill is capped at 10 PUTs a second (about $130/month), however busy the cell. Measured with a
30 ms simulated S3:

| traffic | S3 PUTs per message | storage cost / million messages |
| --- | --- | --- |
| 1 publisher at a time | 1.0 | $5.00 |
| 8 concurrent | 0.13 | $0.62 |
| 64 concurrent | 0.016 | $0.08 |
| 200 tenants, 1 message/s each (shared cell) | 0.05 | $0.26 |

- **Fixed.** An idle cell writes nothing. The default cell (one on-demand and one Spot node,
  0.25 vCPU each) costs about **$9.40/month**. 200 idle tenants used about 2% of a laptop core.
- **Front door.** The Lambda proxy costs about $0.40 per million requests. A Network Load Balancer
  (`CertificateArn` in the template) costs about $16/month instead, plus about $0.02 per million
  requests. It's cheaper above roughly 50 million requests a month.
- **Shared platform.** A shared cell behind the load balancer costs about **$0.30 per million
  messages** (S3 plus load balancer, before egress at $0.09/GB), plus about $26/month for the whole
  cell. That's below Cloud Tasks' $0.40.
- **S3 Express (optional).** The commits alone can go to an S3 Express One Zone bucket
  (`ROLL_TASKS_WAL`, `ExpressZoneId` in the template) at $1.13 instead of $5 per million PUTs.
  The shared cell then comes to about **$0.08 per million messages** before egress, but recent
  commits live in one AZ. It hasn't been tried on real S3 Express yet.
- **DynamoDB layout.** About $6.90 per million messages at any volume.
- **Latency.** A publish waits for its commit: about 100 ms. One publisher sending one message at a
  time gets about 10 a second, so use `/batch` or concurrency. A lower `ROLL_TASKS_COMMIT_MS`
  trades cost for latency.

`roll tasks usage` and the dashboard price your real traffic from metered S3 writes (each tenant
pays its share of the commits it was in).

The alternative DynamoDB layout (`deploy/aws/tasks-dynamodb.yaml`) has no single leader and scales
writers horizontally. See [deploy/aws/README.md](deploy/aws/README.md).

### Tests

`pnpm test:tasks` (or `scripts/test-tasks.sh`) runs the following:

1. Build, including the Lambda feature, and `clippy -D warnings`.
2. Every suite on SQLite, covering:
   - store conformance
   - messages, queues and schedules
   - workflows
   - cluster behaviour: work spreading without duplicates, crash failover, drain hand-back, lease
     extension, API-only nodes
3. Log store tests:
   - conformance on memory and on a directory
   - recovery after a crash
   - group commit and relaxed writes
   - fencing
   - snapshots and pruning
   - fallback past a corrupt snapshot
   - point-in-time restore on a versioned bucket (MinIO): the exact state at a moment, refusal
     while the lease is held, and undoing a restore
   - refusing to open across a gap
   - tenants: isolation, shared commits, deletion, restore, and per-tenant cost shares
   - the cell: crash failover, graceful hand-over, standby forwarding, a deposed leader stepping
     down, zero idle writes, and hosted tenants (admin API, key isolation, hand-over)
4. Every suite again on the log store in memory.
5. In Docker, every suite again on:
   - the log store on S3 (MinIO, versioned like production, real conditional writes)
   - DynamoDB Local with large bodies in S3 (SeaweedFS)

`cargo test -p tasks --release --test cell -- --ignored --nocapture cost_profile` prints the
single-tenant rows of the cost table, and `multi_tenant_cost_profile` the shared-cell row.

It skips itself when nothing tasks-related changed. `--force` runs anyway, and `--unit` skips Docker.
CI is `.github/workflows/tasks.yml` (through `ci.yml` for pull requests and the merge queue). The SDK has its own suite against the real binary:
`pnpm --filter @roll/tasks test`.

## Storage

roll storage is S3-compatible object storage: any S3 SDK or tool (`aws`, boto3, the JS v3 SDK,
rclone, `mc`) works against `rolld` on `:59000`. It adds two things S3 doesn't have: **branching
buckets** (an instant copy-on-write copy of a bucket) and a **recovery window** (deleted and
overwritten objects stay restorable for 7 days by default). Prices are S3 Standard list prices.

```sh
cargo run -p rolld                                        # S3 API on :59000, roll API on :7070
roll storage create assets
roll storage keys create laptop --permission write        # AWS_ACCESS_KEY_ID / _SECRET / AWS_ENDPOINT_URL
aws s3 cp ./logo.png s3://assets/img/logo.png
roll storage branch assets assets-pr-42                   # instant, no data copied
roll storage trash assets                                 # recently deleted, restorable
roll storage usage assets
```

`/dashboard/storage` has buckets, an object browser with drag-and-drop upload, recently deleted
objects, branches, CORS and lifecycle settings, access keys and usage. Docs:
[docs/storage](docs/storage/README.md).

How it works:

- **One object, one blob.** Every customer object is exactly one object in S3, written once and
  never modified. There's no packing and no compaction, so every byte has S3's own durability, and
  roll's code never rewrites stored data.
- **Durable metadata.** Metadata lives in SQLite. Every change goes to S3 as a create-only
  group-commit log entry before the request is acknowledged: the tasks log-store pattern, with
  fencing, snapshots and recovery from S3 alone.
- **Branches.** A branch shares its parent's blobs. A write to a parent first pushes the old state
  down to its branches, so each branch stays frozen at its creation. Creating one costs one row.
- **Delayed deletes.**
  - Deletes and overwrites go to a trash that customers can restore from for `recover_days`.
  - Below that, an unreferenced blob waits 24 hours more before it's really deleted.
  - A daily orphan scan removes blobs left by crashed uploads.
  - A weekly scrub checks that every referenced blob is still in S3.
- **Checksums end to end.** `Content-MD5` and `x-amz-checksum-*` (headers or trailers) are
  verified on the way in, and every upload to S3 carries a CRC32C that S3 checks.
- **Free copies.** `CopyObject` adds a reference instead of copying bytes.
- **Margin without risk.** Blobs are stored in S3 Intelligent-Tiering, so cold data costs roll
  less while customers pay the Standard price (see [pricing notes](docs/storage/pricing.md)).

### Tests

`pnpm test:storage` (or `scripts/test-storage.sh`) runs the following:

1. Build and `clippy -D warnings`.
2. The unit and integration tests of `s3-proto`, `s3-store`, `s3-server` and `s3-control`. These
   cover:
   - AWS's SigV4 test vectors
   - metadata recovery from object storage, fencing, branching, trash, GC
   - the S3 API through the AWS SDK
3. A compatibility suite with real tools in Docker: `amazon/aws-cli`, `rclone/rclone` and
   MinIO's `mc` (`bitnamilegacy/minio-client`).

It skips itself when nothing storage-related changed. `--force` runs anyway, and `--unit` skips
Docker. CI is `.github/workflows/storage.yml`, which also runs the store and server suites
against S3 (SeaweedFS). To do that locally, set `ROLL_E2E_S3_BUCKET` and `ROLL_E2E_S3_ENDPOINT`.
