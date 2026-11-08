# roll queues — architecture

roll queues is a RabbitMQ-compatible message broker written in Rust. It is to RabbitMQ what
Redpanda is to Kafka: the same protocols and semantics, so applications switch by changing the
connection URL, on an architecture built for serverless operation on AWS at the lowest cost.

## Shape

```
              clients (AMQP 0-9-1 / 1.0, MQTT, STOMP, Streams, HTTP)
                                   │  TLS ends on whichever node the NLB picked (the edge)
      ┌────────────────────────────┼────────────────────────────┐
      │ rolld                       │ rolld                     │ rolld         every node of the
      │  protocol front ends        │                           │               roll cloud fleet
      │  mq-core: exchanges, routing, queue partitions (actors) │
      │  routers ──── internal RPC (signed, on 7070) ────► each queue's owner
      └──────────────┬──────────────┴──────────────┬────────────┘
                     │ group-committed appends      │ metadata, change feed
          ┌──────────┴──────────┐         ┌─────────┴──────────┐   DynamoDB: who's alive,
          │ S3 Standard          │         │ the system database │   who owns which queue
          │ (3 AZs, the source   │         │ (or SQLite alone)   │   (roll-cluster leases)
          │  of truth)           │         └────────────────────┘
          └─────────────────────┘
```

- **Nodes hold no state of their own.** A node holds connections, routing tables (cached from
  the metadata store) and the in-memory state of the queue partitions it currently owns. Every
  state change of a durable queue is a record in that partition's log, so any node can rebuild
  a partition by replaying it.
- **Durability is the log's job.** On roll cloud a partition's log is objects in S3 Standard,
  written with put-if-absent and group-committed per node (`mq-storage`, [storage.md](storage.md)):
  a publisher confirm is sent only once the batch holding the message is in S3, which keeps it
  across three availability zones. A node alone keeps logs on local disk.
- **Metadata** (vhosts, users, permissions, exchanges, queues, bindings, policies,
  parameters) lives in a versioned KV table: SQLite on a single node, the system database
  (schema `mq`) on a fleet. The version counter is the change feed every node follows.
- **Partitions and ownership.** A queue is one or more partitions (`x-partitions`, default 1
  for exact RabbitMQ ordering). Each partition is owned by one node, the holder of its
  roll-cluster lease. Losing a node means its leases lapse (about 10 s) and other nodes take
  the partitions over by fencing and replaying the log: no confirmed message is lost, because
  it's in S3.
- **Connections don't move when partitions do.** A client connection stays on the node it
  reached; operations on partitions owned elsewhere are forwarded over the internal RPC.
  Rebalancing moves partition ownership, not client connections, so there is no reconnect storm.
  Stopping a node hands its queues off first, then closes its connections gradually.
- **Scale to zero.** Idle vhosts are unloaded after `--queues-idle-suspend` seconds without
  clients (300 by default): their state stays in the log, and the next connection wakes them,
  like roll postgres wakes computes. See [Idle cost](#idle-cost).

## Crates

| crate         | role                                                                        |
| ------------- | --------------------------------------------------------------------------- |
| `mq-core`     | engine: values, messages, exchanges, queues (classic/quorum/stream actors), routing, dead-lettering, delayed messages, policies, auth, metadata store, log trait |
| `mq-amqp`     | AMQP 0-9-1 server (RabbitMQ wire compatible)                                 |
| `mq-amqp10`   | AMQP 1.0 server (RabbitMQ 4 address format), handed off from the AMQP port   |
| `mq-mqtt`     | MQTT 3.1.1 and 5                                                             |
| `mq-stomp`    | STOMP 1.0–1.2                                                                |
| `mq-stream`   | RabbitMQ Stream protocol (offsets, replay, super streams, dedup)             |
| `mq-mgmt`     | RabbitMQ management HTTP API, definitions, Prometheus metrics                |
| `mq-http`     | HTTP messaging API: publish, long-poll/SSE/WebSocket consume, webhooks, receipts, tokens |
| `mq-shovel`   | shovels, federation and live migration from RabbitMQ                         |
| `mq-storage`  | log stores: objects in S3 (Standard or Express) with group commit and fencing, self-hosted keepers (quorum, S3 tiering) |
| `mq-cluster`  | many nodes: queue ownership on roll-cluster leases, routers, internal RPC, metadata on Postgres |
| `mq-control`  | roll API for the dashboard and CLI, metering and cost, scale-to-zero          |
| `rolld`       | runs everything on each node                                                  |

## Many nodes (`mq-cluster`)

On roll cloud every node runs queues (docs/cloud/README.md); membership, placement, handoff and
takeover are roll-cluster's (docs/code/cluster.md), with DynamoDB holding who's alive and who
owns what. A node alone uses roll-cluster's memory backend and none of this applies: it owns
every queue and its logs are local.

- **Ownership.** Each durable queue partition has a lease, `mq:<vhost>/<queue>` (`#<n>` after the
  first partition; `%`, `/` and `#` in names escaped, so the default vhost is `%2F`). Its holder
  runs the partition's actor and opens the log with the lease's epoch, which fences every
  earlier owner out of the log store: their next append fails instead of being confirmed, and
  an older epoch can't open the log again. Exclusive and transient queues live with their
  connection's node, as on RabbitMQ. Placement is roll-cluster's rendezvous hashing over the live
  nodes; zones don't steer it.
- **Correctness never depends on the lease.** A node that lost its lease without noticing (cut
  off, frozen) keeps nothing: the next owner's open fences the log, so the old one's appends are
  refused and its publishers get nacks (`a_deposed_owner_cannot_confirm`).
- **Routing.** Channels on every node hold a *router* per queue. It forwards to the local actor
  when this node holds the lease, or over the internal RPC otherwise: one multiplexed connection
  per node pair, an HTTP/1.1 upgrade of `GET /internal/queues/rpc` on the other node's internal
  API (`ROLL_ADVERTISE_URL`), signed with the cluster secret like every internal hop; frames carry
  a JSON header plus raw message blobs. Consumers on another node are fed by credit (64 in
  flight), so prefetch semantics hold across nodes. A node that gets a request for a queue it
  doesn't run answers "moved" and, if placement gives the queue to it, starts it; the sender
  routes again.
- **Handoff** (rebalancing, a drain): roll-cluster ends the owner's guard; the router pauses
  deliveries, lets in-flight acks land (1.5 s at most), stops the actor (every write flushed, the
  log closed with its closing marker) and only then lets go of the lease. Routers on other nodes
  get "moved", route again and re-register their consumers with the new owner, so client
  connections and consumer tags never change, and a publish in flight waits for the new owner
  instead of failing.
- **Failover.** When a node dies, its leases lapse after 10 s; the node placement prefers takes
  each one (at its next sweep, or at once when a request for the queue comes), replays the log
  from S3 and serves it. Confirmed messages are in S3, so none are lost. Unacked deliveries
  become ready again, as with RabbitMQ. Meanwhile publishes wait in the routers.
- **Queues that come and go.** A node that takes a lease for a queue of a vhost asleep here wakes
  the vhost. A deleted queue's owner gives its lease up for good (a new queue of that name has a
  new log); a queue that still exists keeps its lease record, because its log refuses epochs
  older than its last owner's and a deleted record starts over at 1.
- **Draining** (SIGTERM: a deploy, scale-in, Spot): rolld's drain (`ROLL_DRAIN_TIMEOUT`) hands every
  queue off while the node keeps serving its clients through the moves; after the 15 s it keeps
  serving for the load balancer, it closes its client connections spread over the rest of the
  drain (5–10 s), so they reconnect through the edge to the other nodes, not all at once.
- **Metadata store.** The system database (`ROLL_SYSTEM_DATABASE`), schema `mq`; one connection
  per node, re-established if it drops. TLS follows the URL's `sslmode` and always verifies the
  server, against the system roots plus `ROLL_QUEUES_DATABASE_CA` (the image ships the RDS CA
  bundle). A metadata database from before (tables in `public`, with `mq_node` and `mq_owner`
  rows) is moved into the schema once and those rows are dropped: roll-cluster has them now.
- **Definitions owned by other crates** (webhooks) follow the same change feed. The broker
  emits `MetaChanged` for kinds it doesn't own, and their owner reconciles. A webhook created
  or changed on one node starts or restarts on every node.
- **Streams.** Stream clients ask `Metadata` for a stream's leader. A stream's owner node is
  reported as its leader and its replica, so publishers and readers connect to it directly.
- **Tests**: `crates/mq-cluster/tests/cluster.rs` (three nodes in one process, on the memory
  backend and on DynamoDB Local, logs on a shared object store) and
  `crates/rolld/tests/queues_cluster_e2e.rs` (Docker: real `rolld` containers with DynamoDB
  Local, Postgres and MinIO; SIGKILL and SIGTERM under load, then a Postgres restart).

## Queue engine

Each queue partition is an actor (`mq-core/src/queue`). Commands are processed in batches: all
pending publishes, acks and settles are applied, and their log records are appended in **one**
write (group commit). Confirms are sent only after that write is durable, so a confirmed
message is never lost, and throughput rises with load instead of falling.

- **Quorum queues** also deliver only what is durable.
- **Classic queues** deliver as soon as a batch is applied, before the write, as RabbitMQ's
  classic queues do. A crash can then redeliver: the publisher was never confirmed, so it
  publishes again. It can never lose a confirmed message.
- **Acks** are written without waiting for an fsync. A lost ack means a redelivery, and the
  next durable write persists it.

- classic and quorum queues share one engine; quorum adds delivery limits (default 20, like
  RabbitMQ 4), persists every message and tracks `x-delivery-count`.
- streams are append-only logs read by offset (`first`, `last`, `next`, number, timestamp,
  interval) with retention by size and age, filters, and an in-memory tail cache.
- the log is compacted by dropping whole segments; long-lived messages that pin an old segment
  are copied forward.

## Idle cost

Idle should cost nothing, at three levels:

- **A vhost** that has had no clients for `--queues-idle-suspend` seconds is unloaded. Its
  queues hold no memory and run no tasks; it costs only its stored bytes until the next
  connection wakes it. Waking reads only that vhost's metadata (`MetaStore::list_vhost`, an
  indexed lookup in Postgres) and opens its queues' logs 32 at a time, each vhost under its own
  lock, so a wake costs about one log open and tenants wake independently. This is what lets
  one warm fleet serve many tenants: tenants scale to zero, the brokers don't have to.
- **Fairness.** Each vhost has a publish budget (`--queues-vhost-publish-rate` and
  `--queues-vhost-publish-bytes-rate` by default, or its `max-publish-rate` and
  `max-publish-bytes-rate` limits). It is a token bucket one second deep that publishers may
  overdraw and then wait off, which slows the busy tenant's channels and, through TCP, its
  clients, instead of everyone's.
- **A broker** with no client connections, no HTTP request in the last 10 s and no running
  shovel, federation link or webhook is *quiet* (`Broker::is_quiet`). Its background loops
  (rate samplers, metering, memory and disk alarms) park until a client arrives, so a quiet
  broker uses no CPU. Per-queue timers are only set when they can do something: stream
  retention when an append or a `max-age` cut is due, and partitioned queues refresh their
  totals every 30 s instead of every 200 ms once they stop changing. Metering covers the
  parked time when it resumes, splitting stored bytes over the days they were stored.
- **The management API** tracks per-object rates only while it (or `/metrics`) has been used in
  the last 10 minutes, like a dashboard that only renders while someone looks at it.

What a loaded queue or an open connection costs is measured by `scripts/scale-queues.sh`
against RabbitMQ 4; see [docs/benchmarks/queues-scale.md](../benchmarks/queues-scale.md).

## Cost model

- Queues share roll cloud's fleet (Graviton, on demand plus Spot); a node holds no state, so a
  Spot interruption is a drain (two minutes' notice).
- Logs are S3 Standard objects ($0.023/GB-month). Appends of every queue on a node share
  objects (group commit), so PUTs follow nodes and traffic (at most one in flight per node, each
  carrying whatever arrived meanwhile), not the number of queues; an idle node makes none.
- Cross-AZ bytes: a client's connection and its queue's owner may be in different zones (the
  internal RPC); S3 is reached through the VPC's gateway endpoint at no transfer cost.
- Metering: messages, GB-months stored, connection-hours. Prices are derived from AWS list
  prices, and the dashboard compares them with Amazon MQ and CloudAMQP for the same workload.
