# roll

Serverless infrastructure for everything.

## Layout

```
apps/
  dashboard/        Next.js 16 app — brand playground (/), dashboard (/dashboard, /dashboard/databases)
packages/
  brand/            @roll/brand — logo geometry, animation engine, React components, SVG assets
crates/
  pg-proxy/         Postgres wire-protocol proxy (routing, wake-on-connect, cancel forwarding)
  pg-control/       Control plane: projects, branches, endpoints, autoscaling, metering, HTTP API
  redis-proto/      RESP2/RESP3 parser and encoder
  redis-engine/     Redis data structures and commands (pure, synchronous, one engine per shard)
  redis-server/     Redis network layer: RESP + TLS, REST API, tenants, durability on S3, replicas
  redis-control/    Redis control plane: databases, branches, restore, replicas, metering, HTTP API
  rolld/            Daemon: proxy + API + reconciler
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

Commands: strings, keys and expiry, lists, hashes (with field TTLs), sets, HyperLogLog, bitmaps,
transactions and `WATCH`, pub/sub (including sharded), ACLs, `CLIENT`, `CONFIG`, `INFO`, `SCAN`, blocking
list ops, eviction policies and keyspace notifications. In progress: sorted sets and geo, streams, Lua
scripting and functions, JSON, and the Bloom family.

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
