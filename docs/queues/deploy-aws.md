# Deploy roll queues on AWS

roll queues runs on roll cloud's fleet: one stack (`deploy/cloud/aws/roll-cloud.yaml`,
[deploy/cloud/README.md](../../deploy/cloud/README.md)), every node running every product
(docs/cloud/README.md). There is no queues stack of its own any more: no broker service, no
front proxies and no keeper instances.

```
 clients ──► Cloudflare DNS ──► NLB (3 zones, TCP passthrough, PROXY protocol v2)
               │ 5671 AMQPS · 8883 MQTTS · 61614 STOMP TLS · 5551 streams · 443 web-STOMP/MQTT, HTTP API
               ▼
 rolld × N (Graviton, on demand + Spot): TLS ends here, the node serves the connection and
               reaches each queue's owner over the internal RPC (signed, port 7070)
   │ leases: who owns which queue        │ metadata               │ logs (group commit)
   ▼                                     ▼                        ▼
 DynamoDB (roll-cluster)        the system database (mq)     S3 Standard: <data bucket>/queues/logs
```

- **Clients** connect to `<id>.mq.<region>.roll.sh` on the TLS ports above; the node the NLB
  picked serves them. Each durable queue runs on the holder of its lease (`mq:<vhost>/<queue>`);
  other nodes forward to it, so a connection never has to move when its queues do.
- **Logs** are objects in the stack's data bucket under `queues/logs` (S3 Standard, three zones),
  written before a publisher is confirmed ([storage.md](storage.md), "On roll cloud").
- **Metadata** (vhosts, users, exchanges, queues, bindings, policies) is in the system database
  (`ROLL_SYSTEM_DATABASE`), schema `mq`.
- **The admin login** is `ROLL_QUEUES_USER` (`roll-admin`) and `ROLL_QUEUES_PASSWORD` (generated in
  Secrets Manager). guest/guest is refused behind the edge.

## Settings that matter for queues

Everything else is the fleet's ([docs/cloud/README.md](../cloud/README.md), "Node settings").

| env | value in the cloud | |
| --- | --- | --- |
| `ROLL_PRODUCTS` | includes `queues` | `ROLL_PRODUCTS=queues` makes a queues-only fleet |
| `ROLL_S3_BUCKET` | the data bucket | logs go under `queues/logs` (`ROLL_QUEUES_STORAGE` overrides: `s3:<bucket>/<prefix>`, `s3express:…`, `quorum:…`) |
| `ROLL_SYSTEM_DATABASE` | the system database | queues' metadata (schema `mq`); a cluster refuses to start without it |
| `ROLL_DRAIN_TIMEOUT` | `90` | how long a stopping node may take to hand its queues off |
| `ROLL_QUEUES_IDLE_SUSPEND` | `300` | a vhost without clients for this long is unloaded; it wakes on its next connection |
| `ROLL_QUEUES_VHOST_PUBLISH_RATE`, `…_BYTES_RATE` | `0` | each vhost's default publish budget (messages/s, bytes/s; 0 = unlimited) |

## A shared fleet

One fleet serves every tenant (vhost). What sleeps is the tenant, not the node:

- **Idle vhosts unload** after `ROLL_QUEUES_IDLE_SUSPEND` without clients and timers: their queues
  flush and stop, and nothing of theirs is in memory. Their data stays in the logs.
- **Waking takes milliseconds.** The next connection, HTTP request or management call loads the
  vhost on the node it reached. The node reads only that vhost's metadata (an indexed lookup,
  however many tenants share the database) and opens its queues' logs in parallel.
- **Tenants can't crowd each other out.** Every vhost has a publish budget: the defaults above, or
  its own `max-publish-rate` / `max-publish-bytes-rate` limits (`rabbitmqctl set_vhost_limits`, or
  `PUT /api/vhost-limits/{vhost}/max-publish-rate` with `{"value": 5000}`; `-1` lifts the
  default). A vhost over budget is slowed down, not refused: its publishers wait, as RabbitMQ's
  flow control does. The vhost's `max-connections` and `max-queues` limits work as in RabbitMQ.

## Operations

- **Deploys and scale-in.** A stopping node (SIGTERM; ECS's stop timeout is 120 s) fails its
  health check, hands every queue it owns to the other nodes while it keeps serving (in-flight
  acks land first, every write is flushed; publishes in flight wait for the new owner), keeps
  serving 15 s more while the NLB takes it out, then closes its client connections spread over
  5–10 s. Clients reconnect through the NLB to the other nodes, with their normal retry.
- **Spot.** The two-minute notice starts the same drain.
- **A node dies.** Its leases lapse after about 10 s; the others take its queues over, replaying
  their logs from S3. Confirmed messages are in S3, so nothing confirmed is lost. Connections to
  the dead node drop and reconnect; connections elsewhere carry on (their publishes wait while the
  queue moves). Unacked deliveries become ready again, as with RabbitMQ.
- **A zone goes down.** The same as its nodes dying: S3 and DynamoDB keep working from the other
  zones.
- **Streams through the NLB.** Stream clients ask for a stream's leader and then connect to it.
  Behind the NLB, use your client's load-balancer mode (`load_balancer_mode(true)` in the Rust
  client, the address resolver in Java).

## What it costs to run

Queues add nothing fixed to the fleet: no instances, volumes or proxies of their own. Per use:
S3 storage ($0.023/GB-month for the retained logs), S3 PUTs (group-committed per node: at most one
in flight per node while it confirms, none while idle; see [storage.md](storage.md)), and the
fleet's share of CPU. The dashboard's Usage page (`/dashboard/queues/usage`) and
`roll queues usage` compare usage with Amazon MQ and CloudAMQP for the same workload.

## Self-hosting

The same `rolld` runs anywhere: one node (`rolld`, logs on local disk, metadata in SQLite), or
several with `ROLL_CLUSTER_BACKEND`/`ROLL_CLUSTER_SECRET` (docs/code/cluster.md), a Postgres
(`ROLL_SYSTEM_DATABASE`) and a bucket. `deploy/queues/Dockerfile` builds a small queues-only image
(with `roll-keeper`, for self-hosted keepers: [storage.md](storage.md)).

## Status

roll cloud's stack passes `cfn-lint` and hasn't run against AWS yet. The pieces are tested
locally and in Docker: many nodes in one process on the memory backend and on DynamoDB Local
(`crates/mq-cluster/tests/cluster.rs`), real `rolld` containers with DynamoDB Local, Postgres and
MinIO under SIGKILL and SIGTERM (`crates/rolld/tests/queues_cluster_e2e.rs`), and the object log
on MinIO (`crates/mq-storage/tests/minio.rs`).
