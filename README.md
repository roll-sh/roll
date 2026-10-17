# roll

Serverless infrastructure for everything.

## Layout

```
apps/
  dashboard/        Next.js 16 app — brand playground (/), dashboard (/dashboard, /dashboard/databases, /dashboard/tasks)
packages/
  brand/            @roll/brand — logo geometry, animation engine, React components, SVG assets
  tasks/            @roll/tasks — TypeScript SDK: client, pull consumer, signature verification, workflows
crates/
  pg-proxy/         Postgres wire-protocol proxy (routing, wake-on-connect, cancel forwarding)
  pg-control/       Control plane: projects, branches, endpoints, autoscaling, metering, HTTP API
  tasks/            roll tasks: messages, queues, schedules, workflows (SQLite, its own S3 log store, or DynamoDB)
  rolld/            Daemon: proxy + API + reconciler (+ roll tasks on /v1/tasks)
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
   - refusing to open across a gap
   - tenants: isolation, shared commits, deletion, restore, and per-tenant cost shares
   - the cell: crash failover, graceful hand-over, standby forwarding, a deposed leader stepping
     down, zero idle writes, and hosted tenants (admin API, key isolation, hand-over)
4. Every suite again on the log store in memory.
5. In Docker, every suite again on:
   - the log store on S3 (MinIO, real conditional writes)
   - DynamoDB Local with large bodies in S3 (SeaweedFS)

`cargo test -p tasks --release --test cell -- --ignored --nocapture cost_profile` prints the
single-tenant rows of the cost table, and `multi_tenant_cost_profile` the shared-cell row.

It skips itself when nothing tasks-related changed. `--force` runs anyway, and `--unit` skips Docker.
CI is `.github/workflows/tasks.yml`. The SDK has its own suite against the real binary:
`pnpm --filter @roll/tasks test`.
