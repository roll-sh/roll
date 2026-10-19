# roll queues — architecture

roll queues is a RabbitMQ-compatible message broker written in Rust. It is to RabbitMQ what
Redpanda is to Kafka: the same protocols and semantics, so applications switch by changing the
connection URL, on an architecture built for serverless operation on AWS at the lowest cost.

## Shape

```
              clients (AMQP 0-9-1 / 1.0, MQTT, STOMP, Streams, HTTP)
                                   │
                    ┌──────────────┴──────────────┐
                    │   mq-proxy (per AZ, tiny)   │  wake-on-connect, AZ-aware routing,
                    └──────────────┬──────────────┘  connection draining
                                   │
      ┌────────────────────────────┼────────────────────────────┐
      │ broker (stateless)          │ broker                    │ broker        Graviton, any AZ
      │  protocol front ends        │                           │
      │  mq-core: exchanges, routing, queue partitions (actors) │
      └──────────────┬──────────────┴──────────────┬────────────┘
                     │ append (quorum) / read       │ metadata, leases, change feed
          ┌──────────┴──────────┐         ┌─────────┴──────────┐
          │ log keepers ×3 AZ   │         │ roll postgres      │
          │ (EBS gp3 segments)  │         │ (or SQLite single) │
          └──────────┬──────────┘         └────────────────────┘
                     │ sealed segments
               S3 Standard (cold tier)  ·  S3 Express One Zone (optional hot backend)
```

- **Brokers are stateless.** A broker holds connections, routing tables (cached from the
  metadata store) and the in-memory state of the queue partitions it currently owns. Every
  state change of a durable queue is a record in that partition's log, so any broker can
  rebuild a partition by replaying it.
- **Durability is the log's job.** `mq-storage` appends each batch to three keepers in three
  AZs and acknowledges on two (a quorum), with epoch fencing so a deposed owner can't write.
  Publisher confirms are sent only after the quorum ack. Sealed segments tier to S3 Standard;
  keepers keep only the hot tail on EBS.
- **Metadata** (vhosts, users, permissions, exchanges, queues, bindings, policies,
  parameters, partition leases) lives in a versioned KV table — SQLite on a single node, roll
  postgres in a cluster. The version counter is the change feed every broker follows.
- **Partitions and placement.** A queue is one or more partitions (`x-partitions`, default 1
  for exact RabbitMQ ordering). Each partition is owned by one broker through a lease. Losing a
  broker means its leases expire (seconds) and other brokers take the partitions over by
  fencing and replaying the log — no message is lost because confirmed messages are on a
  keeper quorum.
- **Connections don't move when partitions do.** A client connection stays on its broker;
  operations on partitions owned elsewhere are forwarded over the internal RPC. Rebalancing
  moves partition ownership, not client connections, so there is no reconnect storm. Rolling
  upgrades drain connections gradually (`Broker::drain`).
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
| `mq-storage`  | replicated log keepers, quorum client with fencing, S3 tiering, S3 Express   |
| `mq-control`  | roll API for the dashboard and CLI, metering and cost, scale-to-zero, cluster placement |
| `mq-proxy`    | wake-on-connect, AZ-aware front proxy                                         |
| `rolld`       | runs everything on one node                                                  |

## Cluster (`mq-cluster`)

- **Membership.** Every broker heartbeats an `mq_node` record (RPC address, AZ, client
  listeners, draining flag). A node is gone when its heartbeat hasn't changed for the TTL (5 s).
- **Ownership.** Each queue partition has an `mq_owner` record `{node, epoch}`, changed only by
  compare-and-set in the metadata store. A new owner bumps the epoch and opens the log with
  it, which fences the previous owner in the log store. Failover winners and rebalance targets
  come from rendezvous hashing over live, non-draining nodes.
- **AZ affinity.** The owner record also carries the queue's home AZ. Placement hashes over the
  live nodes in that AZ first, and only falls back to other AZs when it has none. The owner
  counts operations per source AZ (publishes and deliveries forwarded from other nodes; the
  rest are local). When another AZ has carried at least ⅔ of them, and at least
  `place_min_ops` (1000), for two periods in a row, the home moves there and the queue follows
  with a normal handoff. Clients already land on a broker in their own AZ through the proxy, so a
  queue used from one AZ ends up with no cross-AZ hop between its clients and its owner.
- **Routing.** Channels on every node hold a *router* per queue. It forwards to the local actor
  when this node owns the queue, or over the internal RPC otherwise: one multiplexed TCP
  connection per node pair, with frames carrying a JSON header plus raw message blobs.
  Consumers on another node are fed by credit (64 in flight), so prefetch semantics hold across
  nodes. Nodes prove a shared secret to each other before any RPC
  (`ROLL_QUEUES_CLUSTER_SECRET`, a mutual HMAC handshake). TLS is optional
  (`ROLL_QUEUES_CLUSTER_TLS_CERT`/`_KEY`/`_CA`).
- **Handoff** (rebalance, drain): pause deliveries, let in-flight acks land, stop the actor,
  transfer ownership by CAS, then tell the new owner to adopt. Routers re-register their
  consumers with the new owner, so client connections and consumer tags never change.
- **Failover.** When a node dies, the rendezvous winner claims its queues, replays their logs
  and serves them. Confirmed messages are in the replicated log, so none are lost. Unacked
  deliveries become ready again, as with RabbitMQ.
- **Metadata store.** One Postgres connection per node, re-established if it drops (a restart
  or a Multi-AZ failover). TLS follows the URL's `sslmode` and always verifies the server, against
  the system roots plus `ROLL_QUEUES_CLUSTER_META_CA`. The image ships the RDS CA bundle, so
  `sslmode=require` works with RDS.
- **Definitions owned by other crates** (webhooks) follow the same change feed. The broker
  emits `MetaChanged` for kinds it doesn't own, and their owner reconciles. A webhook created
  or changed on one node starts or restarts on every node.
- **Streams.** Stream clients ask `Metadata` for a stream's leader. A stream's owner node is
  reported as its leader and its replica, so publishers and readers connect to it directly.
- **Rolling upgrades.** SIGTERM drains a broker: queues are handed off first, then
  connections close gradually. Tests: `crates/mq-cluster/tests/cluster.rs` (in-process, three
  nodes) and `crates/rolld/tests/queues_cluster_e2e.rs` (Docker: real `rolld` containers,
  Postgres metadata, SIGKILL and SIGTERM under load, then a Postgres restart).

## Front proxy (`mq-proxy`)

An L4 proxy per AZ. It follows the `mq_node` records and sends each new connection to a
live, non-draining broker in its own AZ with the fewest connections. If no broker in its AZ is
up, it uses one in another AZ. If none is running at all, it holds the connection, runs its
waker (an ECS `UpdateService` signed with SigV4, or a shell command) and forwards once a broker
heartbeats. Load balancer health checks use a separate port, so they never wake brokers.

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

- Graviton (m7g/c7g/c8g) brokers on an on-demand base plus Spot, sized by connections and
  throughput. Brokers are stateless, so a Spot interruption is a drain (two minutes' notice).
- Keepers on gp3 EBS hold minutes of data; everything older is on S3 Standard
  ($0.023/GB-month).
- AZ-aware routing: the proxy sends clients to a broker in their own AZ, and partition leaders
  are placed in the AZ of their busiest producers/consumers, so the only cross-AZ bytes are the
  two replica copies the durability guarantee requires, and those are zstd-compressed.
- With S3 Express storage, appends of every queue on a broker share objects (group commit), so
  request costs follow brokers and traffic, not the number of queues.
- Metering: messages, GB-months stored, connection-hours. Prices are derived from AWS list
  prices, and the dashboard compares them with Amazon MQ and CloudAMQP for the same workload.
