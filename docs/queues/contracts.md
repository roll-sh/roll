# roll queues — internal contracts

Conventions every queues crate follows. Read `AGENTS.md` first.

## Ports

| listener                    | rolld default        | standard (AWS) |
| --------------------------- | -------------------- | -------------- |
| AMQP 0-9-1 + AMQP 1.0       | `127.0.0.1:55672`    | 5672           |
| AMQPS                       | `127.0.0.1:55671`    | 5671           |
| Management API + /metrics   | `127.0.0.1:55673`    | 15672 (+15692) |
| HTTP messaging API          | `127.0.0.1:55680`    | 443            |
| MQTT                        | `127.0.0.1:51883`    | 1883 / 8883    |
| STOMP                       | `127.0.0.1:56613`    | 61613          |
| Streams                     | `127.0.0.1:55552`    | 5552           |
| roll API (`/v1/queues/...`) | rolld API `:7070`    | —              |

Default credentials on first start: `guest` / `guest` on vhost `/` (configurable with
`ROLL_QUEUES_USER` / `ROLL_QUEUES_PASSWORD`).

## Crate shape

A protocol crate exposes:

```rust
pub struct Config { pub listen: SocketAddr, /* … */ }
pub async fn serve(broker: Arc<mq_core::Broker>, config: Config) -> anyhow::Result<()>;
pub async fn serve_listener(broker: Arc<Broker>, listener: tokio::net::TcpListener, config: Config) -> anyhow::Result<()>;
```

An HTTP crate exposes `pub fn router(state) -> axum::Router` (state already applied).
`rolld/src/queues.rs` wires them together.

## The engine API you build on (`mq-core`)

- `Broker::start(BrokerConfig, Arc<dyn MetaStore>, Arc<dyn LogStore>)`; in tests use
  `mq_core::meta::SqliteMeta::open(path)` or `MemMeta::new()`, and `mq_core::log::FileStore::new(dir)`
  or `MemStore::new()`.
- Auth: `broker.authenticate(user, pass, Some(peer))`, `broker.check_vhost_access(&user, vhost)`,
  `broker.check(&user, vhost, Access::Configure|Write|Read, "queue"|"exchange", name)`,
  `broker.check_topic(...)`, `broker.check_connection_limits(&user, vhost)`.
- Vhosts: `broker.open_vhost(name).await` (wakes scaled-to-zero vhosts).
- Topology: `vhost.declare_exchange(..)`, `vhost.declare_queue(DeclareQueue{..})`,
  `vhost.bind/unbind(source, Dest::queue(q) | Dest::exchange(x), key, args, conn)`,
  `vhost.delete_queue(..)`, `vhost.queue_for(name, conn)` (respects exclusivity).
- Publishing: `vhost.publish(exchange, Message, Option<ConfirmTicket>) -> PublishOutcome { routed, confirm }`.
  Settle the returned ticket yourself (`t.settle(true)`) after sending any "returned/unroutable"
  notice; a ticket fires its callback once every queue holding the message has it durably.
  Build messages with `Message::new(exchange, routing_key, Properties, body)`; internal format is
  AMQP 0-9-1 properties — convert at your protocol edge.
- Consuming: implement `ConsumerSink` (never block in it), then
  `queue.consume(ConsumerSpec { tag, channel: broker.next_id(), conn, no_ack, exclusive, priority, args, sink })`.
  Acks: `queue.settle(channel, vec![seq], Outcome::Ack|Requeue|Reject)`. On session end:
  `queue.channel_closed(channel)` (requeues everything that session held). When credit frees up,
  call `queue.wake()`.
- Pull/HTTP: `queue.lease(max, visibility_ms)`, `queue.settle_lease(seq, delivery_count, outcome)`,
  `queue.extend_lease(..)`; `queue.shared.available` is a `Notify` for long-polls.
- Browsing: `queue.peek(offset, count)`.
- Registry: `broker.register_connection(Arc<ConnectionInfo>)` / `broker.connection_closed(id)`
  (deletes exclusive queues), `register_channel` / `channel_closed`. Honour
  `ConnectionInfo::close` (a `Notify`) to support force-close from the management API.
- Metadata for your own state: `broker.meta` (`put/get/list/delete/cas`, use a new kind string).

Changing `mq-core` is allowed only additively (new methods, new fields with defaults) and must
keep `cargo test -p mq-core -p mq-amqp` green.

## Errors

Use `mq_core::AmqpError` codes and RabbitMQ's exact texts where clients or tests look at them
(e.g. `NOT_FOUND - no queue 'q' in vhost '/'`).

## Tests

- Unit tests next to the code. Integration tests in `crates/<crate>/tests/`, starting an
  in-process broker on `127.0.0.1:0` (see `crates/mq-amqp/tests/compat.rs` for the harness).
- Tests that need Docker (real client libraries, RabbitMQ, MinIO) only run when `ROLL_E2E=1`
  and skip otherwise. `scripts/test-queues.sh` runs everything and is change-gated.
- Use real, unmodified client libraries to prove compatibility.

## roll API (`/v1/queues`, served by `mq-control` on the rolld API port)

No auth (like the rest of the local rolld API). JSON. Errors: `{"error": "..."}` with 4xx/5xx.
`{vhost}` is URL-encoded (`%2F` for `/`).

```
GET    /v1/queues/health
GET    /v1/queues/overview                      → Overview
GET    /v1/queues/rates?seconds=300             → { step_s, points: RatePoint[] }       (live graph, 5s samples)
GET    /v1/queues/usage?days=30                 → Usage                                  (metering + cost + comparison)

GET    /v1/queues/vhosts                        → VHost[]
POST   /v1/queues/vhosts                        { name, description?, default_queue_type?, credentials?, user? } → VHost & { credentials? }
       (credentials: true or user: also creates a user with rights on this vhost only, and returns
        { user, password, urls: { amqp, amqps, stream, stomp, mqtt, http, management } }; null for disabled listeners)
POST   /v1/queues/vhosts/{vhost}/credentials    { user? } → { user, password, urls }   (issue or rotate; 409 for a user with other vhosts)
DELETE /v1/queues/vhosts/{vhost}
POST   /v1/queues/vhosts/{vhost}/suspend        (scale to zero now)    POST .../wake

GET    /v1/queues/vhosts/{vhost}/queues         → Queue[]
POST   /v1/queues/vhosts/{vhost}/queues         { name, type?, durable?, auto_delete?, arguments? } → Queue
GET    /v1/queues/vhosts/{vhost}/queues/{q}     → Queue & { consumer_details: Consumer[], bindings: Binding[] }
GET    /v1/queues/vhosts/{vhost}/queues/{q}/rates?seconds=300 → { step_s: 5, points: { t, publish, deliver, ack, ready, unacked }[] }  (last 5 min; empty for idle queues)
DELETE /v1/queues/vhosts/{vhost}/queues/{q}
POST   /v1/queues/vhosts/{vhost}/queues/{q}/purge                → { purged }
GET    /v1/queues/vhosts/{vhost}/queues/{q}/messages?offset=0&count=50 → BrowsedMessage[]   (non-destructive)
POST   /v1/queues/vhosts/{vhost}/queues/{q}/replay  { count?, target_exchange?, target_routing_key? } → { replayed }
       (DLQ replay: re-publishes dead-lettered messages to the exchange/routing key in their x-death history, or the target given)

GET    /v1/queues/vhosts/{vhost}/exchanges      → Exchange[]
POST   /v1/queues/vhosts/{vhost}/exchanges      { name, type, durable?, auto_delete?, internal?, arguments? }
DELETE /v1/queues/vhosts/{vhost}/exchanges/{x}
GET    /v1/queues/vhosts/{vhost}/bindings       → Binding[]
POST   /v1/queues/vhosts/{vhost}/bindings       { source, destination, destination_type, routing_key?, arguments? }
DELETE /v1/queues/vhosts/{vhost}/bindings       (same body)
POST   /v1/queues/vhosts/{vhost}/publish        { exchange, routing_key, body, body_encoding?: "utf8"|"base64", properties? } → { routed }

GET    /v1/queues/connections                   → Connection[]
DELETE /v1/queues/connections/{id}
GET    /v1/queues/channels                      → Channel[]
GET    /v1/queues/policies                      → Policy[]
PUT    /v1/queues/policies/{vhost}/{name}       { pattern, apply_to, definition, priority }
DELETE /v1/queues/policies/{vhost}/{name}
GET    /v1/queues/users                         → User[]
PUT    /v1/queues/users/{name}                  { password?, tags? }
DELETE /v1/queues/users/{name}
PUT    /v1/queues/permissions/{vhost}/{user}    { configure, write, read }
GET    /v1/queues/definitions                   → RabbitMQ definitions JSON
POST   /v1/queues/definitions                   (same body) → { ok: true, skipped: Skipped[] }
POST   /v1/queues/migrate                       { source: { management_url, username, password, vhost?, amqp_uri? }, move_messages?: boolean, include_shovels?: boolean }
                                                → { imported: { vhosts, exchanges, queues, bindings, policies, users }, shovels: string[], skipped: Skipped[] }
GET    /v1/queues/migrate                       → { shovels: MigrationShovel[] }
DELETE /v1/queues/migrate                       → { stopped: string[] }   (removes every roll-migrate-* shovel and the source credentials it holds)
```

Migration (`mq-shovel`, wired into `mq-control` by rolld; 501 when not wired):

- `source.amqp_uri` (optional) is where the shovels reach the source over AMQP, e.g.
  `amqps://rabbit.example.com:5671` when its port is remapped (NAT, load balancer, Docker).
  Without it, the host of `management_url` and the port from the source's `/api/overview`
  are used, preferring the `amqp/ssl` listener when `management_url` is https. Credentials
  default to `username` / `password`; the URI's path is ignored (each queue's vhost is used).
- `include_shovels` (default false): also import the source's own `shovel` and
  `federation-upstream` parameters. Off by default because they would start running on roll;
  they are listed in `skipped` instead.
- Imports skip what roll can't take and keep going; `skipped` says what and why. Policies lose
  classic-mirroring keys (`ha-*`) and `queue-master-locator` (removed in RabbitMQ 4; reported
  with `partial: true`), and are skipped when nothing else is left. Unknown policy keys,
  patterns Rust's `regex` can't compile (e.g. look-ahead), objects the broker rejects and
  bindings to skipped objects are skipped. Shovel and federation parameters are validated.
  Queues that weren't imported get no shovel. Only metadata-store failures fail the request.
- A shovel is drained when it is `running` and `remaining` is 0. `remaining` is null until the
  shovel has read the source queue; `terminated` means it failed (it retries after
  `reconnect-delay`) and `reason` says why. Shovels keep draining (producers may still publish
  to the source) until the migration is stopped.

Types (TypeScript notation; numbers are JSON numbers, times are unix seconds unless `_ms`):

```ts
interface Overview {
  version: string; node: string; cluster_name: string; uptime_s: number;
  endpoints: { amqp?: string; amqps?: string; mqtt?: string; stomp?: string; stream?: string; http?: string; management?: string };
  totals: { vhosts: number; sleeping_vhosts: number; connections: number; channels: number; exchanges: number; queues: number; consumers: number;
            messages_ready: number; messages_unacked: number; stored_bytes: number };   // sleeping vhosts count with their last-known totals
  rates: { publish: number; deliver: number; ack: number };            // msg/s, last 5s window
  storage: { backend: string; metadata: string };
}
interface RatePoint { t: number; publish: number; deliver: number; ack: number; ready: number; unacked: number; connections: number }
interface VHost { name: string; description: string; default_queue_type: string; state: "running" | "sleeping";
  sleeping: boolean;    // scaled to zero: queues, messages and bytes are its last-known totals (kept on disk)
  connections: number; queues: number; messages_ready: number; messages_unacked: number; stored_bytes: number;
  rates: { publish: number; deliver: number }; limits: Record<string, number>; amqp_url: string }
interface Queue { name: string; vhost: string; type: "classic" | "quorum" | "stream"; durable: boolean; auto_delete: boolean;
  exclusive: boolean; arguments: Record<string, unknown>; policy: string | null; state: "running";
  messages_ready: number; messages_unacked: number; messages: number; consumers: number; bytes: number;
  rates: { publish: number; deliver: number; ack: number; redeliver: number };
  dead_letter_exchange: string | null; dead_lettered: number; is_dlq: boolean;   // something dead-letters into it
  first_offset?: number; next_offset?: number }                                   // streams
interface Consumer { consumer_tag: string; channel: number; connection: number; ack_required: boolean; exclusive: boolean; priority: number; active: boolean; prefetch: number }
interface Exchange { name: string; vhost: string; type: string; durable: boolean; auto_delete: boolean; internal: boolean;
  arguments: Record<string, unknown>; bindings: number; rates: { publish_in: number; publish_out: number } }
interface Binding { source: string; destination: string; destination_type: "queue" | "exchange"; routing_key: string; arguments: Record<string, unknown> }
interface BrowsedMessage { seq: number; exchange: string; routing_key: string; redelivered: boolean; delivery_count: number;
  properties: Record<string, unknown>; payload: string; payload_encoding: "string" | "base64"; payload_bytes: number;
  x_death?: { queue: string; reason: string; count: number; exchange: string; routing_keys: string[] }[] }
interface Connection { id: number; name: string; protocol: string; vhost: string; user: string; peer: string; tls: boolean;
  channels: number; connected_at_ms: number; recv_oct: number; send_oct: number; client: string | null }
interface Channel { id: number; connection: number; number: number; vhost: string; user: string; prefetch: number;
  unacked: number; consumers: number; confirm: boolean; transactional: boolean }
interface Policy { vhost: string; name: string; pattern: string; apply_to: string; definition: Record<string, unknown>; priority: number; operator: boolean }
interface User { name: string; tags: string[]; permissions: { vhost: string; configure: string; write: string; read: string }[] }
interface Skipped { kind: string;            // user | permission | topic_permission | parameter | policy | exchange | queue | binding | shovel
  vhost?: string; name: string; reason: string;
  partial?: boolean }                          // imported, without what `reason` describes (e.g. dropped ha-* keys)
interface MigrationShovel { name: string; vhost: string; state: "starting" | "running" | "terminated"; reason: string | null;
  src: string | null; dest: string | null;     // src: "<queue> @ <uri without credentials>"
  transferred: number; remaining: number | null }
interface Usage {
  period: { start: number; end: number; days: number };
  messages: { published: number; delivered: number; usd: number };
  storage: { current_bytes: number; gb_months: number; usd: number };
  connections: { hours: number; usd: number };
  total_usd: number; projected_month_usd: number;
  pricing: { usd_per_million_messages: number; usd_per_gb_month: number; usd_per_connection_hour: number; basis: string };
  compare: { name: string; plan: string; monthly_usd: number; note: string }[];   // Amazon MQ, CloudAMQP for the same workload
  daily: { day: number; published: number; delivered: number; stored_bytes: number; connection_hours: number; usd: number }[];
}
```
