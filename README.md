# roll

Serverless infrastructure for everything.

## Layout

```
apps/
  dashboard/        Next.js 16 app — brand playground (/), dashboard (/dashboard, /dashboard/databases, /dashboard/streams)
packages/
  brand/            @roll/brand — logo geometry, animation engine, React components, SVG assets
crates/
  pg-proxy/         Postgres wire-protocol proxy (routing, wake-on-connect, cancel forwarding)
  pg-control/       Control plane: projects, branches, endpoints, autoscaling, metering, HTTP API
  stream-log/       Kafka-format partition log: segments, group commit, idempotence, transactions
  stream-broker/    Kafka-compatible broker: wire protocol, coordinators, tiering, compaction, HTTP API
  rolld/            Daemon: proxy + API + reconciler + Kafka listener
  roll-cli/         `roll` CLI
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
