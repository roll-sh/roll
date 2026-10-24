# roll code on many nodes

roll code runs on one node with SQLite, or on any number of identical nodes behind a load
balancer. In a cluster, every node runs the same `rolld --code-only` with the same Postgres
database and the same object storage (S3). Nodes can be added, drained and killed at any time:
an acknowledged write is never lost, and a node's work moves to the others within seconds.

```
                    load balancer (any node, no stickiness)
          ┌───────────────────┼───────────────────┐
        node A              node B              node C        rolld --code-only
          │  repo requests are forwarded to the repo's owner  │
          └───────────────────┼───────────────────┘
        Postgres: metadata, leases, the landing log      S3: file contents
```

## What lives where

| state | single node | cluster |
| --- | --- | --- |
| changesets, refs, git mapping, path index, landing log (`code-store`) | SQLite `code/code.db` | Postgres schema `code_store` |
| owners, repos, changes, reviews, work, queue, jobs, deliveries (`code-control`) | SQLite `code/control.db` | Postgres schema `code_control` |
| file contents | object storage | S3 (required: a local directory isn't shared) |
| code index (search, symbols) | `code/index/` | each repo's owner, rebuilt by a new owner |
| work trees, preview containers | local | the repo's owner |

Both databases go through `crates/code-sql`: SQL is written once in SQLite's dialect and
translated for Postgres. A write is one transaction (serializable on Postgres, re-run on
conflicts) and is durable when it returns, on both backends. Postgres gives the cluster
point-in-time recovery and backups.

## Nodes

Each node has an id (random per process, `rolld --code-node-id` to pin it) and an internal URL
that other nodes reach it at (`--code-advertise-url`, e.g. `http://10.0.1.5:7070`). It
heartbeats a `code_nodes` row every second from a thread of its own, so a busy async runtime
can't make it look dead. A node is alive while its heartbeat is younger than 5 s, measured on the
database's clock, so node clocks don't matter.

Nodes talk to each other over HTTP on the API port, under `/internal/code/`. Every internal
request is signed with the cluster secret (`--code-cluster-secret`, HMAC-SHA256 over the node,
a timestamp, the method and the path; stale or unsigned requests are 401s). TLS between nodes
is whatever the network provides; put the cluster on a private network.

## Repo ownership

Every repo with activity has one owner node, held through a lease row in `code_repo_owners`
(`repo_id, node_id, epoch, expires_at`). Leases last 10 s and are renewed every 2 s with one
statement per node (`UPDATE … WHERE node_id = me`).

- **Requests for a repo run on its owner.** Every route under `/v1/code/repos/{owner}/{repo}`,
  `/git/{owner}/{repo}.git`, and the routes that name a change, thread or work item, are
  forwarded by whichever node receives them to the repo's owner, streaming in both directions
  (git pushes, event streams). MCP tools call the API router, so they are forwarded the same
  way. A node that isn't the owner never runs a repo's lander, queue, index or previews.
- **Acquiring.** A request for a repo nobody owns (or whose owner's lease expired) is taken by the
  node the repo's rendezvous hash picks among the live nodes; the receiving node forwards it
  there, and the winner acquires the lease with a compare-and-swap that bumps `epoch`.
- **Losing it.** A node only acts as an owner while its last successful renewal is younger than
  the lease minus a safety margin, on its own monotonic clock. A node cut off from Postgres
  stops serving its repos (503 with `Retry-After`) before anyone else can take them.
- **Correctness doesn't depend on leases.** Every ref update is a compare-and-swap on the ref
  row, and every read-modify-write is one serializable transaction, so two nodes briefly
  believing they own a repo can't corrupt anything: the loser's landing finds the ref moved,
  reloads and rebases. Leases are about locality: caches, the lander's batching, the code index,
  containers.
- **Taking over.** Each node sweeps for expired leases every second and takes the repos the
  rendezvous hash gives it. Taking a repo over drops every cache the node held for it, then
  resumes its work from the database: queued and running merge-queue batches, AI jobs that were
  running on the dead node, mirrors, checks on the default branch, previews (restarted), and the
  code index (rebuilt in the background; search answers 503 until the first build finishes, as
  on a fresh repo).
- **Joining and draining.** When a node joins, the others hand it the repos the rendezvous hash
  now gives it, a few at a time: the owner stops taking new requests for the repo (forwarders
  retry), waits for the ones in flight and for the lander to go idle, then deletes its lease. A
  node shutting down (SIGTERM) drains every repo it owns the same way before exiting, so rolling
  upgrades don't wait for lease expiry.

## Work that isn't per repo

- **Webhook and review-app deliveries** are claimed row by row (`FOR UPDATE SKIP LOCKED` with a
  claim that expires), so every node delivers and none twice.
- **Landings to webhooks** come from the durable landing log. On Postgres, concurrent
  transactions could take sequence numbers in one order and commit in another, so `code-store`
  appends under a transaction-scoped advisory lock taken just before the append (the landing's
  last statements): numbers become visible in order, and a reader paging by `seq` skips nothing
  and reads nothing twice. (Reading only rows older than `pg_snapshot_xmin` isn't enough by
  itself: a transaction still running can hold a lower number than a committed row whose
  transaction id is older.) One node at a time runs the forwarder (a lease), and the cursor
  moves in the same transaction as the deliveries it queues.
- **AI spend caps** reserve their price in a `ai_reservations` row inside the same serializable
  transaction that checks the cap, so concurrent jobs on different nodes can't overshoot it.
  Reservations of a dead node expire.
- **MCP sessions** live in the database, so a session works through any node.
- **GitHub webhooks** are verified and de-duplicated by whichever node receives them, then
  handled on the owner of each repo they concern.
- **Event-stream limits** (`--code-max-streams-per-caller`, `--code-max-streams`) are per node.

## Running a cluster

```
rolld --code-only \
  --code-database 'postgres://roll:…@db.internal:5432/roll?sslmode=require' \
  --code-cluster-secret "$ROLL_CODE_CLUSTER_SECRET" \
  --code-advertise-url "http://$(hostname -i):7070" \
  --s3-bucket roll-code --s3-region eu-west-1
```

- `GET /v1/code/health` answers 200 while the node can reach Postgres and holds its leases, for
  the load balancer's health check.
- Postgres sizing: each node opens at most `--code-db-connections` connections per database (16
  by default). Use a session-mode pooler or none: prepared statements and `SET search_path` need
  session state.
- Every node must use the same object storage, the same `ROLL_SECRET_KEY` (sealed owner keys) and
  the same `ROLL_CODE_TOKEN`.
