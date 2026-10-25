# roll code in production

roll code runs as any number of identical `rolld --code-only` nodes behind an HTTP load balancer,
sharing one Postgres database and one S3 bucket. Nodes can be added, drained and killed at any
time: an acknowledged write is never lost, and a node's repos move to the others within seconds.
How it works is in [docs/code/cluster.md](../../docs/code/cluster.md).

```
             clients: git, the roll CLI, agents (MCP), the dashboard, CI
                                    │
                     HTTP load balancer (no stickiness)
          ┌─────────────────────────┼─────────────────────────┐
       node A                    node B                    node C       rolld --code-only
          │      requests for a repo are forwarded to its owner node    │   (this image)
          └─────────────── private network, port 7070 ────────────────┘
        Postgres: metadata, refs, leases, the landing log      S3: file contents
```

- `Dockerfile`: one small image (Debian slim, non-root user `roll`, tini, CA certificates plus
  Amazon RDS's bundle, a health check on `GET /v1/code/health`). Build it from the repository
  root: `docker build -f deploy/code/Dockerfile -t roll-code .`
- `docker-compose.yml`: three nodes, Postgres, SeaweedFS (S3) and nginx as the load balancer, to
  try a cluster on one machine.
- `crates/rolld/tests/code_cluster_e2e.rs`: the end-to-end test of this image as a cluster (kills,
  a rolling restart and a Postgres restart under load; `ROLL_E2E=1`).

## Try it locally

```sh
docker build -f deploy/code/Dockerfile -t roll-code .
docker compose -f deploy/code/docker-compose.yml up -d
curl -s localhost:7070/v1/code/health                      # through the load balancer
export ROLL_API=http://localhost:7070 ROLL_TOKEN=dev-admin-token-change-me
roll code owners create acme --org && roll code create acme/web
git clone http://git:$ROLL_TOKEN@localhost:7070/git/acme/web.git
docker compose -f deploy/code/docker-compose.yml kill node2   # keeps working
docker compose -f deploy/code/docker-compose.yml down -v
```

`ROLL_CODE_PORT` changes the published port (7070). The secrets in the compose file are
placeholders: never reuse them.

## What every node needs

Every node runs the same image with the same settings, except its advertise URL (and node id, if
you pin one).

| setting | |
| --- | --- |
| `ROLL_CODE_DATABASE` | `postgres://user:password@host:5432/db?sslmode=verify-full`. Both of roll code's databases live in it (schemas `code_store` and `code_control`, created on first start). |
| `ROLL_S3_BUCKET`, `AWS_REGION` | The bucket for file contents. Credentials come from the instance or task role (or `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`). `ROLL_S3_PREFIX` keeps roll code under a prefix; `ROLL_S3_ENDPOINT` points at another S3-compatible store. |
| `ROLL_CODE_CLUSTER_SECRET` | At least 16 bytes, the same on every node. Nodes sign every request between them with it. Without it, the node runs alone. |
| `ROLL_CODE_ADVERTISE_URL` | `http://<this node's private address>:7070`: where the other nodes reach this one. Each node's own. |
| `ROLL_CODE_TOKEN` | The admin token, the same on every node. Without one the API is open: never run a reachable node without it. |
| `ROLL_SECRET_KEY` | At least 32 characters, the same on every node: it seals owners' own model API keys. Losing it makes those keys unreadable (owners enter them again). |
| `ROLL_CODE_PUBLIC_URL` | The load balancer's URL (`https://code.example.com`), for links and clone URLs. `ROLL_CODE_GIT_URL` overrides the clone URL base. |
| `ROLL_CODE_NODE_ID` | Optional. A stable id per node (a StatefulSet ordinal, an instance id). Without it each process gets a random one. A restarted process with a pinned id waits until its previous incarnation looks dead (about 5 s). |
| `ROLL_CODE_DRAIN_TIMEOUT` | Seconds a stopping node waits for its repos to move (30). |
| `ROLL_CODE_DB_CONNECTIONS` | Connections per database per node (16). See sizing. |
| `ROLL_CODE_DB_CA` | Extra CA certificates for Postgres. The image sets it to the RDS bundle (`/etc/roll/rds-global-bundle.pem`). |

The image sets `ROLL_CODE_ONLY=1`, `ROLL_HOME=/data` and `ROLL_API=0.0.0.0:7070`. Everything
else (`--help` lists it) works as on a single node: the reviewer (`ROLL_REVIEW_*`), the GitHub App
(`ROLL_GITHUB_*`), stream limits and the git memory budget, which are per node.

A node refuses to start in a cluster on SQLite or on a local object directory, since neither is
shared.

## The network

- The load balancer sends any request to any node; no stickiness is needed. Every node answers
  every route and forwards a repo's requests to the node that owns it. Use HTTP (layer 7) or TCP
  (layer 4) balancing. Turn off request and response buffering for `/git/` and `/events` (pushes
  and clones stream; event streams are long-lived) and allow request bodies of at least 64 MiB
  (API writes) and as large as your biggest push. Give `/events` a long idle timeout (an hour or
  more) and the rest at least 60 s: a node holds a request for up to ~13 s while its repo moves.
- Nodes reach each other on port 7070 at their advertise URLs. Internal routes are signed, but
  their bodies aren't encrypted: keep that traffic on a private network (a VPC, a security group
  that only admits the nodes and the load balancer).
- Terminate TLS at the load balancer. Behind a proxy that sets `X-Forwarded-For`,
  `ROLL_CODE_TRUST_FORWARDED_FOR=1` counts anonymous event streams per client rather than per proxy.
- Webhooks go out from every node: allow egress to your receivers. They only go to public
  addresses unless `ROLL_CODE_WEBHOOK_ALLOW_PRIVATE=1` (for receivers on your own network; never on
  a server that tenants share).

## Health checks

`GET /v1/code/health` needs no token. It answers 200 while the node's database answers and the
node holds its leases, and 503 (`Retry-After: 1`) when it can't trust its leases (Postgres is
unreachable) or is shutting down. Point the load balancer's health check at it: interval 5 s,
unhealthy after 2 failures, healthy after 2 successes. The image's Docker `HEALTHCHECK` calls it
too. Don't make the orchestrator *restart* a node only because it's unhealthy for a short while:
during a Postgres failover every node is unhealthy until it reconnects, by itself, within seconds.

## Rolling upgrades and scaling

- **Stopping a node** (SIGTERM, which tini passes on): it fails its health check at once, keeps
  serving, hands every repo it owns to the other nodes (waiting for requests in flight, queue
  batches and reviews), then closes its listener, finishes the requests it's still serving (for
  what's left of the drain timeout, between 5 and 10 s; open event streams are cut then, and
  their clients reconnect) and exits 0. Set the orchestrator's stop timeout above
  `ROLL_CODE_DRAIN_TIMEOUT` plus 10 s: ECS `stopTimeout` 60 (at most 120), Kubernetes
  `terminationGracePeriodSeconds: 60`, `docker stop -t 60`, compose `stop_grace_period: 60s`.
- **Take the node out of the load balancer before SIGTERM.** A node that owns few repos drains in
  well under a second and closes its port, often before the load balancer's health check has
  noticed. ECS services deregister a task from its target group and wait for the deregistration
  delay before they stop it; set that delay to a few seconds (5–10). On Kubernetes, endpoint
  removal and SIGTERM happen at the same time: add a `preStop` hook that sleeps for about your
  ingress's propagation time (`sleep 10`; the image has `/bin/sleep`). Otherwise the load
  balancer may send requests to a closed port (most retry those elsewhere) or on keep-alive
  connections the node is closing (POSTs among them can come back 502 without having run).
- **Rolling upgrade**: replace one node at a time, waiting for the new one to be healthy (it sees
  the others and takes its share of the repos within seconds). A request for a repo that's
  moving waits for it.
- **A node that dies** (SIGKILL, a lost host): its repos move when their leases run out, within
  about 10–15 s. Requests for them through other nodes wait and are answered then; nothing
  acknowledged is lost. Requests that were on the dead node's own connections fail, and so may
  requests another node was passing on to it (502): whether they ran is unknown.
- **Clients should retry** connection errors, timeouts and 5xx answers on another node (or
  through the load balancer), with backoff, honouring `Retry-After`. A landing that may have run
  is safe to retry when its content identifies it: a retry of a landing that did land conflicts
  (409) on the files it added. `crates/rolld/tests/code_cluster_e2e.rs` uses exactly this policy
  through a kill, a rolling restart and a Postgres restart, and no operation fails.
- **Webhooks** are delivered at least once. A node that dies just after sending a delivery
  leaves it to be sent again when its claim lapses (60 s later), under the same `X-Roll-Delivery`
  id: de-duplicate on it.
- **Scaling** out or in is the same: start nodes, or stop them with SIGTERM. A new node gets its
  share of the repos (rendezvous hashing), a few at a time.
- Nodes create and migrate their schemas on start (each migration runs once, recorded in
  `code_migrations`). Keep to one version of roll except during the upgrade itself.

## Postgres

- Any managed or self-run Postgres (tested on 18): RDS, Aurora PostgreSQL, roll postgres, or your
  own.
- **Session pooling only.** Connect directly, or through a pooler in session mode (PgBouncer
  `pool_mode = session`). Transaction pooling breaks prepared statements and `SET search_path`.
- **Sizing.** Each node opens at most `ROLL_CODE_DB_CONNECTIONS` connections for each of its two
  databases plus 4 for leases: 2 × 16 + 4 = 36 with the defaults. Allow `nodes × 36` plus room
  for a rolling upgrade's extra node and your own tools: three nodes need about 150
  `max_connections`. Lower `ROLL_CODE_DB_CONNECTIONS` (8 is plenty for most loads) for more nodes.
  Writes are serializable transactions; a database with a few vCPUs serves thousands of landings a
  minute.
- A Postgres failover or restart doesn't need the nodes restarted: they reconnect, and requests in
  the meantime answer 503 with `Retry-After`. Nodes stop acting as owners when they can't renew
  their leases, so nothing runs twice.

## S3

- One bucket (or prefix) for the whole cluster, in the database's region. Blobs are written before
  the metadata that refers to them, so the database never points at a missing object.
- Give the nodes `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject` and `s3:ListBucket` on it.
- Block public access; use SSE-S3 or SSE-KMS (transparent to roll).

## Backups and recovery

- **Postgres**: point-in-time recovery (RDS automated backups with a retention period, or WAL
  archiving). All metadata is there: owners, repos, refs, changesets, changes, reviews, tokens.
- **S3**: turn on bucket versioning, with a lifecycle rule that expires noncurrent versions after
  your PITR window plus a margin (e.g. 35 days). File contents are content-addressed and never
  rewritten, so versioning costs almost nothing; it protects against deletes.
- **Restoring**: restore Postgres to a point in time, keep the bucket as it is (or restore its
  versions to the same time or later; newer objects than the database are harmless), and start
  the nodes on the restored database. Local disk (`/data`) is a cache: work trees and the code
  index are rebuilt.
- Keep `ROLL_SECRET_KEY` and `ROLL_CODE_TOKEN` in your secret manager with the backups.

## Checks and previews

roll's own checks and previews (`roll.toml`) run in containers through a Docker daemon on the
node that owns the repo. This image has no Docker CLI and nodes shouldn't hold a Docker socket,
so on these nodes checks and previews don't run; report CI results with commit statuses
(`POST /v1/code/repos/{owner}/{repo}/statuses/{rev}`, docs/code/http-api.md), which gate the merge
queue the same way.

## Local state

`/data` holds caches only (the code index, work trees). A volume avoids rebuilding the index after
a restart but isn't required; it never needs a backup.
