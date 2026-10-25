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

The pieces: `crates/code-cluster` (membership, leases with self-fencing, placement, handoff,
forwarding; generic over string keys) and `crates/code-control/src/cluster.rs` (which requests
and which work belong to a repo, and starting and stopping that work as ownership moves).

## What lives where

| state | single node | cluster |
| --- | --- | --- |
| changesets, refs, git mapping, path index, landing log (`code-store`) | SQLite `code/code.db` | Postgres schema `code_store` |
| owners, repos, changes, reviews, work, queue, jobs, deliveries (`code-control`) | SQLite `code/control.db` | Postgres schema `code_control` |
| nodes and leases (`code_nodes`, `code_leases`) | — | Postgres schema `code_control` |
| file contents | object storage | S3 (required: a local directory isn't shared) |
| code index (search, symbols) | `code/index/` | each repo's owner, rebuilt by a new owner |
| work trees, check and preview containers | local | the repo's owner |

Both databases go through `crates/code-sql`: SQL is written once in SQLite's dialect and
translated for Postgres. A write is one transaction (serializable on Postgres, re-run on
conflicts) and is durable when it returns, on both backends. Postgres gives the cluster
point-in-time recovery and backups. One node can also use Postgres without a cluster
(`--code-database` without `--code-cluster-secret`).

## Nodes

Each node has an id (random per process, `--code-node-id` to pin it) and an internal URL that
other nodes reach it at (`--code-advertise-url`, default `http://<--public-host>:<API port>`).
It heartbeats a `code_nodes` row every second from a thread of its own, so a busy async runtime
can't make it look dead. A node is alive while its heartbeat is younger than 5 s, measured on the
database's clock, so node clocks don't matter.

Nodes talk to each other over HTTP on the API port, under `/internal/code/`. Every internal
request is signed with the cluster secret (`--code-cluster-secret`, HMAC-SHA256 over the node,
a timestamp, the method and the path; stale or unsigned requests are 401s). The body isn't
signed and TLS between nodes is whatever the network provides: put the cluster on a private
network.

| internal route | what it does |
| --- | --- |
| `GET /internal/code/ping` | the node's id |
| `POST /internal/code/repos/{id}/events` | events emitted on another node, for the repo's feed (`/events`) on its owner. Best effort, like the feed itself: nothing durable travels this way |
| `POST /internal/code/repos/{id}/kick` | the owner resumes the repo's work (a queue to run, a GitHub repo connected or disconnected, GitHub delivery parts recorded for it). The work is in the database; the kick is retried for about two minutes until some node takes it, and whoever takes the repo next resumes it anyway |
| `POST /internal/code/repos/{id}/github` | handle one recorded part of a GitHub delivery (`{part, claimed_until}`: a `github_inbox` row) |

Each `repos/{id}` route is itself routed by the repo id, so it follows the repo if it moved
while the request was on its way.

## Repo ownership

Every repo with activity has one owner node, held through a lease row in `code_leases`
(`key, node_id, epoch, expires_at`; the key is the repo id, so a rename keeps the owner). Leases
last 10 s and are renewed every 2 s with one statement per node (`UPDATE … WHERE node_id = me`).

- **Requests for a repo run on its owner.** Whichever node receives a request forwards it to the
  repo's owner, streaming in both directions (git pushes and packs, event streams), with the
  original path, method and headers, signed, carrying the client's address. The owner verifies
  the signature (`code_cluster::accept_forwarded` on the router) and counts anonymous event
  streams against that address. A node that isn't the owner never runs a repo's lander, queue,
  reviews, index, mirror or containers.

  | requests | routed by |
  | --- | --- |
  | `/v1/code/repos/{owner}/{repo}` and everything under it | the repo's id (renames followed) |
  | `/git/{owner}/{repo}.git/…` | the repo's id (renames followed) |
  | `/v1/code/changes/{id}/…` | the change's repo |
  | `/v1/code/threads/{id}/…` | the thread's repo |
  | `/v1/code/work/{id}/…` | the work item's repo |
  | MCP tool calls (`/v1/code/mcp`) | the routes they call, routed as above (MCP calls the API router in process) |
  | preview hosts (`chg-<id>.<preview domain>`) | the change's repo |
  | everything else: info, health, names, owners, tokens, the repo list and creating repos, pricing, review apps, GitHub setup, installations and webhooks | runs on the node that got it |

  A name or id that doesn't exist runs where it arrived (its 404). The owner holds a guard for
  as long as the response body is being sent, so a handoff waits for a clone to finish; an event
  stream instead ends as soon as its repo starts moving, and its client reconnects to the new
  owner (the dashboard and `roll code watch` do). HTTP/1.1 upgrades (WebSockets to a preview) are
  forwarded too: the owner switches protocols, and the node the client reached copies the bytes
  both ways until either side closes. A forwarded read (`GET`, `HEAD`, `OPTIONS`) whose
  connection drops before an answer comes (a pooled connection closed under it, the owner going
  away) is sent again; a write never is.
- **Acquiring.** A request for a repo nobody owns (or whose owner's lease expired) is taken by the
  node the repo's rendezvous hash picks among the live nodes; the receiving node forwards it
  there, and the winner acquires the lease with a compare-and-swap that bumps `epoch`.
- **Riding out moves.** While a repo has no owner that can serve it (a handoff in progress, or
  its owner died and its lease hasn't expired), the node the client reached waits and routes the
  request again, for about a lease's length (13 s), so failovers and rebalancing don't fail
  requests. Bodies up to 4 MiB are kept for this; a bigger or streamed one (a large `git push`)
  gets one try and may answer `503` with `Retry-After`. Only "unavailable" answers (nothing ran)
  and a `508` from two nodes briefly disagreeing about placement are retried, and a node never
  forwards a request back to the node it came from.
- **Losing it.** A node only acts as an owner while its last successful renewal is younger than
  the lease minus a safety margin, on its own monotonic clock. A node cut off from Postgres
  stops serving its repos (503 with `Retry-After`) before anyone else can take them. Work in
  flight checks the lease it started under ten times a second and stops when it's gone.
- **Correctness doesn't depend on leases.** Every ref update is a compare-and-swap on the ref
  row, and every read-modify-write is one serializable transaction, so two nodes briefly
  believing they own a repo can't corrupt anything: the loser's landing finds the ref moved,
  reloads and rebases. Leases are about locality: caches, the lander's batching, the code index,
  containers.
- **Taking over.** Each node sweeps for expired leases every second and takes the repos the
  rendezvous hash gives it. A release (a handoff, a node leaving) expires a lease rather than
  deleting it, so a repo with work left always gets an owner again, and its background work is
  never stranded.
- **Giving repos up.** A lease row is deleted, so no sweep takes the repo back, in two cases. A
  deleted repo's owner (which runs the delete) retires its lease once the request is done: `Lost`
  stops its work, and the row goes. And an owner gives up a repo it has served no request for and
  run no work for in 30 minutes (`Config::repo_idle_release`; an open event stream counts as use),
  if nothing is waiting for it: no merge-queue entries, reviewer jobs, checks queued or running,
  running previews, promised webhook events or recorded GitHub delivery parts, and no GitHub
  connection (its mirror). It stops granting guards while it checks, so nothing can start
  meanwhile. The next request for the repo (or a kick) takes it again, on its preferred node,
  and resumes its work from the database as any takeover does. So the lease table and each
  renewal stay the size of the active repos.
- **Joining and draining.** When a node joins, the others hand it the repos the rendezvous hash
  now gives it, a few at a time: the owner stops taking new requests for the repo (forwarders
  retry), waits for the ones in flight and for queue batches and reviews running under the
  repo's guard (up to 30 s), then expires its lease. A node shutting down (SIGTERM) fails its
  health check, drains every repo it owns the same way (`--code-drain-timeout`, 30 s) while it
  keeps serving, and only then exits, so rolling upgrades don't wait for lease expiry. Before it
  exits it closes its listener and lets the requests it's still serving finish (those it was
  passing on to other nodes; for what's left of the drain timeout, between 5 and 10 s, after
  which open event streams are cut), so a request that reached it is answered rather than cut.
  Set the stop timeout above the drain timeout plus 10 s.

## A repo's work, on its owner

Taking a repo over (`Ownership::Acquired`) first drops everything the node cached about it
(settings, review configs and facts, languages, readmes, blames, landing changes, and the like:
`Code::forget_repo`), then resumes its work from the database (`Code::resume_repo`, idempotent):

| work | taken over | given up (`Lost`) |
| --- | --- | --- |
| merge queue | a worker starts if entries are active; it first recovers the batch the last owner left (a batch that was landing counts as landed if its tip is on the branch; the rest is tested again) | the worker is aborted (each batch runs under the repo's guard, so a handoff waits for it first) |
| roll's reviewer | jobs `running` under another node's claim are queued again; queued ones run here (each runs under the repo's guard) | running jobs are aborted and left `running` for the next owner |
| GitHub | the import if it never finished, else the mirror | the mirror and import stop |
| code index | an index this node still has catches up; a repo whose reviewer is on builds one in the background; others build on first use (search answers 503 until it's built, as on a fresh repo) | removed from disk if the repo hasn't come back after two lease lengths (a node that fenced itself through a slow database usually gets it straight back); a node starting in a cluster removes every index it finds (it owns nothing yet), and a single node those of repos that no longer exist; deleting a repo removes its index on its owner at once |
| checks (code-preview) | checks that were queued or running on open changes run again; the default branch's head is checked unless it's the last green one (which opens or closes `broken-main` items) | results of checks still running are dropped |
| previews (code-preview) | previews stored as running are asleep (their containers were elsewhere); the next visit starts one here | the containers stop |

`Lost` can come late (a node frozen past its deadline) or never (a killed process), so every
piece of this also checks `Code::owns_repo` before its side effects, and long work runs under
`Code::while_owner`, which drops it once the lease it started under is gone. On a single node
`owns_repo` is always true and none of this changes anything.

Every background loop that used to scan all repos at startup only works on the repos this node
owns: `start_queues`, the reviewer's job runner, the indexer, GitHub mirrors and imports,
code-preview's check recovery and preview reconciliation.

A reviewer job is claimed by the node that runs it (`ai_jobs.claimed_by`, `lease_until` 60 s
ahead on the database's clock, renewed every 20 s while it runs). Its node finishes or retries it
only while it still holds the claim, and stops it if the claim was taken. A node starting up
re-queues nothing on a cluster (a single node still re-queues every running job: they were all
its own); instead each node re-queues, every 5 s, the jobs of the repos it owns whose claim
lapsed (their node died or lost the database), and a node taking a repo over re-queues the
repo's running jobs at once. So restarting one node never runs another node's review twice.

Events emitted on a node that doesn't own the repo (a global route) are sent to the owner's feed
over the internal events route, in order per repo, and dropped after a few seconds if no node can
take them (like an event sent while nobody listens). Nothing durable depends on them: webhook
deliveries are queued before `emit` returns, queue and GitHub work is in the database (kicks only
make it start sooner), and reviewer jobs are rows the owner's runner polls.

## Work that isn't per repo

Everything below is shared through the database, so it holds with any number of nodes, and a
node that dies mid-way leaves nothing behind that the others can't finish.

- **Webhook and review-app deliveries** (`review_deliveries`) are sent by every node's delivery
  worker, which starts with the node's webhooks (so deliveries a previous process queued go out
  without waiting for a new event). A worker claims a due delivery with a compare-and-swap on its
  `next_at`, which moves it a claim's length (20 s: a request's 10 s timeout plus a margin) ahead
  on the database's clock, so each delivery is sent by one node at
  a time. A node that dies holding a claim leaves the delivery due again when the claim lapses,
  and any node retries it then. The claim is far longer than a request's total timeout (10 s,
  response included), and a request only starts while at least the timeout plus a margin (5 s)
  of the claim is left, measured on the node's monotonic clock from before the claim was taken;
  a node stalled past that gives the claim back unsent. Recording an attempt is conditional on
  still holding the claim. So a slow receiver can't make two nodes send at once, and if a stall
  longer than all of that ever did, nothing is overwritten and the possible second send is
  logged. Receivers still de-duplicate on `X-Roll-Delivery` (a timed-out attempt may have
  arrived).
- **Events to webhooks.** `Code::emit` queues an event's deliveries (one write, nothing when the
  repo has no webhook for it) before it returns, on whichever node emits it. The state changes
  that matter most also promise their event in their own transaction: a change opened, queued,
  closed or landed (directly or by the merge queue), a review submitted, a check or a commit
  status recorded write a `webhook_outbox` row when the repo has an active webhook for the event,
  and emitting the event turns the row into deliveries (deleting it, in one write). A node that
  dies between the commit and the event leaves the row; the node running the land forwarder
  delivers rows older than 30 s (a single node: every row, when it starts), with the change or
  review as it is then, and deleting the row decides who delivers it, so it's delivered once.
  Other events (threads, work items, previews, queue state, pushes, `review` updates such as a
  dismissal, and the `change` events that accompany them) are queued when emitted, so they're
  lost only if their node dies in the milliseconds between the state change and the `emit`.
- **Landings to webhooks** come from the durable landing log. On Postgres, concurrent
  transactions could take sequence numbers in one order and commit in another, so `code-store`
  appends under a transaction-scoped advisory lock taken just before the append (the landing's
  last statements): numbers become visible in order, and a reader paging by `seq` skips nothing
  and reads nothing twice. (Reading only rows older than `pg_snapshot_xmin` isn't enough by
  itself: a transaction still running can hold a lower number than a committed row whose
  transaction id is older.) One node at a time runs the forwarder
  (`Code::singleton("land-forwarder")`, checked before every page, so a node that loses it stops
  within a page). Each page is one transaction that checks the cursor is still where the page
  started (a compare-and-swap), reads the webhooks of the page's repos, queues their deliveries
  and moves the cursor past the page. A crash mid-page leaves nothing of it; a second forwarder
  racing the first queues nothing and re-reads from the new cursor. Creating a webhook reads the
  cursor in its own transaction and starts the webhook at the cursor or the log's head,
  whichever is further, so a webhook gets exactly the landings after its `since_seq`. Every
  landing wakes the forwarder on every node (Postgres `NOTIFY` sent in the landing's
  transaction, so it arrives after the commit; `code_sql::Db::listen` keeps a connection
  listening and wakes it again after a reconnect), so a landing on one node reaches webhooks
  within milliseconds when another node forwards; a poll every 5 s remains the safety net. The
  `landed` event's `change` is known to the node that landed it; the forwarder looks it up when
  it's another node.
- **Which webhooks an event goes to** is read from the database for each event and each page of
  landings; nothing about webhooks is cached on a node. A webhook created, changed or deleted
  through any node applies to events on every node at once. (`Code::forget_repo_webhooks` exists
  for nodes taking a repo over; there's nothing to drop today.)
- **AI spend caps** reserve a job's estimated price in an `ai_reservations` row inside the
  transaction that checks the cap, which counts the month's metered spend plus every node's
  reservations. Reservations for one owner take turns on the owner's `ai_owner_locks` row (read
  committed, so a busy owner's jobs wait in line rather than failing serialization over and over)
  and read spend and holds in one statement, one snapshot. Metering a job writes its usage and
  deletes its reservation in one transaction, so it's never counted twice nor missed; a job that
  ends without being metered deletes it. A running job renews its row every third of its
  lifetime (`AiConfig::reservation_ttl_ms`, 5 minutes); a dead node's rows lapse then and stop
  counting. Free public reviews are reserved the same way.
- **MCP sessions** live in `mcp_sessions`, so a session started through any node works through
  every other and survives restarts. `last_seen` is written at most every 5 s; a session idle
  for 7 days ends, and past 10,000 sessions the least recently used go.
- **GitHub webhooks** are verified and de-duplicated by whichever node receives them, and each
  connected repo's part is recorded (`github_inbox`, in the transaction that records the
  delivery id) before anything runs. Events about the App (installations) are handled there;
  each repo's part runs on the repo's owner (`/internal/code/repos/{id}/github`), under a claim on
  its row that's renewed while it runs. GitHub gets 2xx only once every part is handled or
  recorded: handled in time, 200; the handler failed on a live owner, the delivery's rows and its
  id are forgotten and GitHub gets the error (its redelivery runs it all again); the owner
  couldn't be reached or died mid-way, 202, and the parts stay; still running after 8 s, 202. The
  owner of a repo handles its recorded parts on taking it over, when kicked, and every 5 s
  (failures again with backoff, given up after 8 attempts). A node that dies holding a part
  leaves it due when its claim (20 s) lapses. Parts run at least once: one handled by a node that
  died before deleting its row runs again, and the handlers are idempotent (keyed by GitHub's
  ids).
- **Event-stream limits** (`--code-max-streams-per-caller`, `--code-max-streams`) are per node.
- **The git memory budget** (`--code-git-memory-budget`) is per node.

## Running a cluster

```
rolld --code-only \
  --code-database 'postgres://roll:…@db.internal:5432/roll?sslmode=require' \
  --code-cluster-secret "$ROLL_CODE_CLUSTER_SECRET" \
  --code-advertise-url "http://$(hostname -i):7070" \
  --s3-bucket roll-code --s3-region eu-west-1
```

| flag (env) | default | |
| --- | --- | --- |
| `--code-only` (`ROLL_CODE_ONLY`) | off | run only roll code: no Postgres control plane or proxy, Redis, streams, tasks or queues |
| `--code-database` (`ROLL_CODE_DATABASE`) | SQLite under `--home` | Postgres for both databases (schemas `code_store`, `code_control`) |
| `--code-db-connections` (`ROLL_CODE_DB_CONNECTIONS`) | 16 | connections per database per node (plus 4 for leases in a cluster) |
| `--code-db-ca` (`ROLL_CODE_DB_CA`) | system roots | more CA certificates (PEM) for Postgres; TLS follows the URL's `sslmode` |
| `--code-cluster-secret` (`ROLL_CODE_CLUSTER_SECRET`) | none: one node | join a cluster; at least 16 bytes, the same on every node; needs `--code-database` and S3 |
| `--code-advertise-url` (`ROLL_CODE_ADVERTISE_URL`) | `http://<public host>:<API port>` | where the other nodes reach this one (`http://`, no path) |
| `--code-node-id` (`ROLL_CODE_NODE_ID`) | random per process | a pinned id; a restarted process waits until the old one looks dead |
| `--code-drain-timeout` (`ROLL_CODE_DRAIN_TIMEOUT`) | 30 | seconds SIGTERM waits for this node's repos to move before it exits |
| `--code-git-memory-budget` (`ROLL_CODE_GIT_MEMORY_BUDGET`) | 1024 | MiB every git request together may hold |
| `--code-git-max-request-memory` (`ROLL_CODE_GIT_MAX_REQUEST_MEMORY`) | ¾ of the budget | MiB one git request may hold |
| `--code-git-memory-wait-secs` (`ROLL_CODE_GIT_MEMORY_WAIT_SECS`) | 30 | how long a git request waits for memory before it fails as busy |

- A cluster node refuses to start on SQLite or on a local object directory: neither is shared
  between nodes.
- `GET /v1/code/health` answers 200 while the node's database answers and (in a cluster) it holds
  its leases and isn't draining; else 503 with `Retry-After: 1` (docs/code/http-api.md). Point
  the load balancer's health check at it.
- Postgres sizing: each node opens at most `--code-db-connections` connections per database (16
  by default) and 4 for its leases. Use a session-mode pooler or none: prepared statements and
  `SET search_path` need session state.
- Every node must use the same object storage, the same `ROLL_SECRET_KEY` (sealed owner keys) and
  the same `ROLL_CODE_TOKEN`.

## Limits

- Events forwarded to an owner are best effort (as the feed itself is: a stream that isn't open
  misses them). Anything that must not miss a landing reads the landing log.
- Events that aren't promised in their state change's transaction (threads, work items,
  previews, queue state, pushes, review dismissals) are lost to webhooks if their node dies in
  the moment between the state change and `emit` (their deliveries are durable from `emit` on).
- A repo connected to GitHub is never given up as idle (its mirror runs on its owner).
