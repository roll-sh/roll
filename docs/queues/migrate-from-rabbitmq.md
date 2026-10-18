# Migrate from RabbitMQ

roll queues speaks the RabbitMQ protocols and management API, and presents itself to clients
as RabbitMQ 4.1. Applications move by changing the connection URL. The topology, users and
messages move with one of three cutovers:

| option                                                       | use it when                                                       |
| ------------------------------------------------------------ | ----------------------------------------------------------------- |
| [A. Live migration](#a-live-migration)                       | roll can reach the source's management API and AMQP port (set `--amqp-uri` if that port is remapped). One command. |
| [B. Definitions plus shovels](#b-definitions-export-and-import-plus-shovels) | you want to review the topology first, or need per-shovel settings. |
| [C. Federation](#c-federation-for-a-gradual-move)            | you move services one at a time over days, with both brokers live. |

The examples use a local `rolld` and a source broker at `rabbit.internal`. On hosted roll,
replace `127.0.0.1` with `<your-endpoint>`.

```sh
export ROLL_API_URL=http://127.0.0.1:7070     # roll API, used by the `roll` CLI
export ROLL_MGMT=http://127.0.0.1:55673       # roll's RabbitMQ management API
export SRC=http://rabbit.internal:15672       # source management API
```

## Before you start

- **Use an administrator on the source.** Live migration reads `GET /api/definitions`, which
  needs the `administrator` tag. The same user also connects over AMQP to move messages, so it
  needs `read` on every queue you move.
- **Check reachability.** roll must reach the source's management port (usually 15672) and,
  to move messages, its AMQP port (5672, or 5671 for `amqps`).
- **Check password hashes.** Imported users keep their password hashes. roll checks
  `rabbit_password_hashing_sha256` and `rabbit_password_hashing_sha512`. Users with other
  hashes, such as MD5 from very old RabbitMQ, can't log in; reset them with
  `roll queues users passwd <name>`.
- **Plugins and auth backends.** roll authenticates against its own user store only (SASL
  `PLAIN` and `AMQPLAIN`). LDAP, OAuth 2.0 and x.509 (`EXTERNAL`) users aren't in the
  definitions and have no roll equivalent. Create them as roll users.

## A. Live migration

Live migration does two things:

1. It reads the source's `/api/definitions` and imports the vhosts, users (with their password
   hashes), permissions, topic permissions, parameters, policies, exchanges, queues and
   bindings.
2. With `move_messages`, it creates one dynamic shovel per queue, named
   `roll-migrate-<queue>`. Each shovel consumes the source queue and publishes into the
   queue of the same name on roll, with `ack-mode: on-confirm` and `src-delete-after: never`.
   Stream queues are copied from offset `first`.

The shovels keep running until you delete them. Messages published to the source after the
import keep flowing to roll, so you can move clients in any order.

### With the CLI

```sh
export ROLL_MIGRATE_USER=admin ROLL_MIGRATE_PASSWORD=...

roll queues migrate --from $SRC --move-messages
# imported from http://rabbit.internal:15672: 3 vhosts, 12 exchanges, 40 queues, 51 bindings, 4 policies, 6 users
# moving messages with 40 shovel(s)
# follow with `roll queues migrate --watch`

roll queues migrate --watch      # live progress per shovel, until every one has drained
roll queues migrate              # one snapshot
roll queues migrate --stop       # remove the migration shovels (and the source credentials)
```

- `--user` / `--password` (or `ROLL_MIGRATE_USER` / `ROLL_MIGRATE_PASSWORD`) are required with
  `--from`.
- `--vhost <name>` (or `ROLL_QUEUES_VHOST`) migrates a single vhost. RabbitMQ's per-vhost
  export has no users or permissions, so create those separately.
- Without `--move-messages`, only the definitions are imported.
- `--amqp-uri amqps://rabbit.example.com:5671` (or `ROLL_MIGRATE_AMQP_URI`) sets where the
  shovels reach the source over AMQP. Use it when the port the source reports isn't the one
  you connect to (a load balancer, NAT, a Docker port mapping). See
  [where the shovels connect](#where-the-shovels-connect).
- `--include-shovels` also imports the source's own shovels and federation upstreams. They
  are left out by default because they would start running on roll.
- Anything roll can't import is skipped and listed with the reason; the rest is imported.
  See [what the import does](#what-the-import-does).
- `--watch` shows each shovel as `waiting` (it hasn't read the source queue yet), `moving`,
  `drained` (running, source queue empty) or `failed` (with its reason; it retries). It stops
  once every shovel has drained.
- The dashboard has the same flow under **Queues → Migrate**, with an optional **AMQP URI**
  field and a **Stop migration** button.

### With the API

```sh
curl -X POST $ROLL_API_URL/v1/queues/migrate -H 'content-type: application/json' -d '{
  "source": {"management_url": "http://rabbit.internal:15672", "username": "admin", "password": "..."},
  "move_messages": true
}'
```

```json
{ "imported": { "vhosts": 3, "exchanges": 12, "queues": 40, "bindings": 51, "policies": 3, "users": 6 },
  "shovels": ["roll-migrate-orders", "roll-migrate-emails"],
  "skipped": [
    { "kind": "policy", "vhost": "/", "name": "ha-all", "reason": "nothing left to import: ha-mode, ha-sync-mode are obsolete in RabbitMQ 4 (classic queue mirroring was removed; replicate with quorum queues)" },
    { "kind": "parameter", "vhost": "/", "name": "shovel/legacy", "reason": "the source's shovels aren't imported by default because they would start running on roll (set include_shovels to import them)" }
  ] }
```

Add `"vhost": "<name>"` to `source` to migrate one vhost, and `"amqp_uri": "amqps://…"` to
set where the shovels connect. `"include_shovels": true` also imports the source's shovels
and federation upstreams. The username and password default to `guest` when omitted.

Progress:

```sh
curl $ROLL_API_URL/v1/queues/migrate
```

```json
{ "shovels": [ { "name": "roll-migrate-orders", "vhost": "/", "state": "running", "reason": null,
  "src": "orders @ amqp://rabbit.internal:5672/%2F", "dest": "orders", "transferred": 1200, "remaining": 0 } ] }
```

`remaining` is the source queue's ready count, as the shovel last saw it, and `null` until the
shovel has read the source. `state` is `starting`, `running` or `terminated`. A `terminated`
shovel with a `reason` failed and is retrying with backoff. A shovel has drained when it is
`running` with `remaining: 0`. `GET $ROLL_MGMT/api/shovels` shows the same shovels in
RabbitMQ's format, and `DELETE $ROLL_MGMT/api/shovels/vhost/{vhost}/{name}/restart` restarts one.

Stop the migration (removes every `roll-migrate-*` shovel and the source credentials stored in
them):

```sh
curl -X DELETE $ROLL_API_URL/v1/queues/migrate
# { "stopped": ["roll-migrate-orders", "roll-migrate-emails"] }
```

### Where the shovels connect

By default the shovels connect to the management URL's host, on the port the source reports
in `/api/overview` `listeners`: the `amqp/ssl` listener when the management URL is https,
otherwise `amqp` (each falls back to the other). If clients reach the source on a different
host or port, pass it explicitly:

```sh
roll queues migrate --from $SRC --move-messages --amqp-uri amqps://rabbit.example.com:5671
```

The URI's credentials default to `--user` / `--password`. Its path is ignored: each shovel
uses its queue's vhost.

### Cutover order

1. Run the migration with `--move-messages`.
2. Point consumers at roll. The shovels feed them what producers still send to the source.
3. Point producers at roll.
4. Wait until every shovel shows `remaining: 0` (`roll queues migrate --watch` exits then) and
   the source has no publishers.
5. Stop the migration. This deletes the migration shovels and the source credentials stored
   in them:

   ```sh
   roll queues migrate --stop
   ```

6. Decommission the source.

Moving producers first also works. Source consumers and the shovel then share the source
backlog until the consumers move.

## B. Definitions export and import, plus shovels

### 1. Export from the source

```sh
curl -u admin:... $SRC/api/definitions -o definitions.json
# or: rabbitmqadmin export definitions.json
```

Review the file. See [What the import does](#what-the-import-does) for what roll rejects.

### 2. Import into roll

```sh
roll queues definitions import definitions.json
# or: curl -X POST $ROLL_API_URL/v1/queues/definitions -H 'content-type: application/json' -d @definitions.json
# or: curl -u guest:guest -X POST $ROLL_MGMT/api/definitions -H 'content-type: application/json' -d @definitions.json
```

The import is idempotent. Existing objects are kept, and equivalent redeclarations do
nothing, so you can fix the file and run it again.

### 3. Move messages with shovels

Create one dynamic shovel per queue on roll. The parameters use RabbitMQ's keys. `amqp://`
without a host means the local roll broker.

```sh
curl -u guest:guest -X PUT $ROLL_MGMT/api/parameters/shovel/%2F/move-orders \
  -H 'content-type: application/json' -d '{
  "value": {
    "src-protocol": "amqp091",
    "src-uri": "amqp://admin:...@rabbit.internal:5672/%2F",
    "src-queue": "orders",
    "dest-protocol": "amqp091",
    "dest-uri": "amqp://",
    "dest-queue": "orders",
    "ack-mode": "on-confirm",
    "src-delete-after": "never"
  }
}'
```

- `src-delete-after` also accepts `"queue-length"`, which moves the messages that were in the
  queue when the shovel started and then deletes the shovel. It also accepts a message count.
- Other supported keys: `src-exchange` / `src-exchange-key`, `dest-exchange` /
  `dest-exchange-key`, `src-prefetch-count` (default 1000), `src-consumer-args`,
  `dest-add-forward-headers`, `dest-add-timestamp-header`, `dest-publish-properties` and
  `reconnect-delay` (default 5 s).
- For a stream queue, add `"src-consumer-args": {"x-stream-offset": "first"}`.
- `amqps://` source URIs work; the default port is 5671.

Watch them with `GET $ROLL_MGMT/api/shovels`, then follow the [cutover order](#cutover-order)
above.

## C. Federation for a gradual move

Federation lets roll pull from the source while both brokers serve traffic. Configure it on
roll, as you would on RabbitMQ.

1. Define the source as an upstream:

   ```sh
   curl -u guest:guest -X PUT $ROLL_MGMT/api/parameters/federation-upstream/%2F/source \
     -H 'content-type: application/json' -d '{
     "value": {"uri": "amqp://admin:...@rabbit.internal:5672", "ack-mode": "on-confirm"}
   }'
   ```

   Upstream keys: `uri` (one or a list), `queue`, `exchange`, `prefetch-count` (default 1000),
   `reconnect-delay`, `max-hops` (default 1), `expires`, `message-ttl`, `ack-mode` and
   `trust-user-id`.

2. Apply it with a policy:

   ```sh
   roll queues policies set federate --pattern '^orders' --apply-to queues \
     --definition '{"federation-upstream-set": "all"}'
   ```

   Only one policy applies to a queue (the highest priority). If imported policies already
   match these queues, for example to set a dead-letter exchange, copy their keys into the
   federation policy.

3. Check the links: `curl -u guest:guest $ROLL_MGMT/api/federation-links`.

How the links behave:

- **Queue federation.** The roll queue consumes from the upstream queue of the same name (or
  the upstream's `queue`), and only while the roll queue has consumers and nothing ready for
  them. Move consumers one service at a time. Each one drains the source as it needs
  messages. HTTP `receive` long-polls aren't consumers, so they don't trigger the pull; use
  SSE, a WebSocket or an AMQP consumer.
- **Exchange federation.** roll declares a `federation: …` queue and an internal fanout
  exchange on the upstream, mirrors the local exchange's bindings onto the upstream exchange,
  and republishes what arrives on roll. Messages published to the source exchange reach roll's
  bindings too. The source user needs `configure` permission for this.

When nothing is left on the source, delete the policy and the upstream, then switch the
producers.

## What the import does

The import (live, CLI, roll API or management API) applies objects in this order: users,
vhosts, permissions, topic permissions, global parameters, parameters, policies, exchanges,
queues, bindings.

- **Users** with the same name as a roll user replace it, including `guest`.
- **Parameters** in a definitions file include the source's own shovels and federation
  upstreams, and they start running on roll. Live migration leaves `shovel` and
  `federation-upstream` parameters out unless you ask for them (`--include-shovels`), and
  validates the ones it imports.
- **Policies** are validated as RabbitMQ does it. Classic-mirroring keys (`ha-mode`,
  `ha-params`, `ha-sync-mode`, `ha-sync-batch-size`, `ha-promote-on-*`) and
  `queue-master-locator` are dropped, because RabbitMQ 4 removed them; the rest of the policy
  is imported, and a policy with nothing else is skipped. Policies with other unknown keys or
  invalid values are skipped. Patterns are compiled with Rust's `regex`, so policies (and
  permissions) whose patterns use look-around are skipped.
- **Exchanges** named `amq.*` and the default exchange are skipped. Supported types: `direct`,
  `fanout`, `topic`, `headers`, `x-consistent-hash`, `x-random`, `x-local-random`,
  `x-delayed-message` (needs `x-delayed-type`) and `x-message-deduplication`. Any other type
  fails with `unknown exchange type`.
- **Queues** of type `classic`, `quorum` and `stream` are supported. Any other `x-queue-type`
  is skipped. Live migration starts no shovel for a skipped queue.

Objects roll rejects are skipped, along with bindings to them, and the import carries on. The
response (`skipped`: kind, vhost, name and reason; `partial: true` when the object was
imported with changes), `roll queues migrate`, `roll queues definitions import` and the
dashboard list them. Only a failure of roll's metadata store stops an import. The management
API's `POST /api/definitions` answers 204 as RabbitMQ does and logs what it skipped.

## Client connection strings

Only the URL changes. The vhost goes in the path, URL-encoded: `/` is `%2F`. These clients
are tested against roll with their unmodified libraries (`tests/queues/clients`,
`crates/mq-amqp/tests`).

Local default: `amqp://guest:guest@127.0.0.1:55672/%2F`. Hosted: `amqps://<user>:<pass>@<your-endpoint>:5671/<vhost>`.

| client             | before                                                        | after                                                          |
| ------------------ | ------------------------------------------------------------- | -------------------------------------------------------------- |
| pika (Python)      | `pika.URLParameters("amqp://u:p@rabbit:5672/%2F")`             | `pika.URLParameters("amqp://u:p@127.0.0.1:55672/%2F")`          |
| amqplib (Node)     | `amqp.connect("amqp://u:p@rabbit:5672/%2F")`                   | `amqp.connect("amqp://u:p@127.0.0.1:55672/%2F")`                |
| amqp091-go         | `amqp.Dial("amqp://u:p@rabbit:5672/%2F")`                      | `amqp.Dial("amqp://u:p@127.0.0.1:55672/%2F")`                   |
| Java client        | `factory.setUri("amqp://u:p@rabbit:5672/%2F")`                 | `factory.setUri("amqp://u:p@127.0.0.1:55672/%2F")`              |
| .NET client        | `new ConnectionFactory { Uri = new Uri("amqp://u:p@rabbit:5672/%2F") }` | `… new Uri("amqp://u:p@127.0.0.1:55672/%2F")`          |
| Bunny (Ruby)       | `Bunny.new("amqp://u:p@rabbit:5672/%2F")`                      | `Bunny.new("amqp://u:p@127.0.0.1:55672/%2F")`                   |
| lapin (Rust)       | `Connection::connect("amqp://u:p@rabbit:5672/%2F", props)`     | `Connection::connect("amqp://u:p@127.0.0.1:55672/%2F", props)`  |
| Celery             | `broker_url = "pyamqp://u:p@rabbit:5672/%2F"`                  | `broker_url = "pyamqp://u:p@127.0.0.1:55672/%2F"`               |
| Spring AMQP        | `spring.rabbitmq.host=rabbit`, `spring.rabbitmq.port=5672`     | `spring.rabbitmq.host=127.0.0.1`, `spring.rabbitmq.port=55672`  |
| MassTransit        | `cfg.Host("rabbit", 5672, "/", h => { … })`                    | `cfg.Host("127.0.0.1", 55672, "/", h => { … })`                 |

`roll queues url` prints the URL of a vhost:

```sh
roll queues url               # amqp://guest:guest@127.0.0.1:55672/%2F
roll queues url --vhost app
```

The URL is for roll's default user. It includes the password only while that password is
still `guest`. Put your application user and password in it.

Other protocols change the same way. MQTT moves to port 51883 (WebSocket: 55675 `/ws`), STOMP
to 56613 (WebSocket: 55674 `/ws`), Stream protocol clients to 55552, and the management API to
55673.

## Feature parity

| RabbitMQ                                     | roll queues                                                                 |
| -------------------------------------------- | --------------------------------------------------------------------------- |
| AMQP 0-9-1, confirms, transactions, QoS      | supported; global QoS and transient non-exclusive queues are still allowed (as in RabbitMQ 4.1) |
| AMQP 1.0                                     | on the AMQP port, RabbitMQ 4 address format                                 |
| classic, quorum and stream queues            | supported. Quorum queues default to a delivery limit of 20, as in RabbitMQ 4. |
| mirrored classic queues (`ha-*` policies)    | dropped from imported policies (RabbitMQ 4 removed them); every queue is durable in the replicated log |
| queue arguments                              | `x-message-ttl`, `x-expires`, `x-max-length`, `x-max-length-bytes`, `x-overflow`, `x-dead-letter-exchange`, `x-dead-letter-routing-key`, `x-dead-letter-strategy`, `x-delivery-limit`, `x-single-active-consumer`, `x-max-priority`, `x-consumer-timeout`, `x-queue-mode` (accepted), `x-max-age`, `x-stream-max-segment-size-bytes` |
| delayed message exchange plugin              | `x-delayed-message` exchange with the `x-delay` header                      |
| message deduplication plugin                 | `x-message-deduplication` exchange, or a queue with `x-message-deduplication: true`; `x-deduplication-header`, `x-cache-size`, `x-cache-ttl` |
| consistent hash and random exchanges         | `x-consistent-hash`, `x-random`, `x-local-random`                           |
| shovel and federation plugins                | dynamic shovels and federation upstreams, configured as parameters (AMQP 0-9-1 endpoints only) |
| management API                               | RabbitMQ 4 paths and JSON. `rabbitmqadmin`, the Terraform `rabbitmq` provider, `rabbit-hole` and `pyrabbit2` work against port 55673. |
| management web UI                            | not served; use the roll dashboard                                          |
| `rabbitmq_prometheus`                        | `/metrics`, `/metrics/per-object` and `/metrics/detailed` on the management port, with the plugin's metric names |
| MQTT plugin                                  | MQTT 3.1, 3.1.1 and 5.0, retained messages, `vhost:user` usernames          |
| STOMP plugin                                 | STOMP 1.0–1.2 with RabbitMQ's destinations                                  |
| stream plugin (Stream protocol)              | its own listener (local default 55552): offsets, replay, super streams, deduplication |
| —                                            | roll additions: `x-partitions` (a queue over N partitions), the [HTTP API](http-api.md), and DLQ replay (`roll queues replay`) |

## Differences / not supported

- **Auth backends.** No LDAP, OAuth 2.0 or x.509 client certificates. roll users only.
- **Password hashes.** Only SHA-256 and SHA-512 hashes are checked.
- **Plugin exchange types** other than those listed above fail to declare.
- **MQTT QoS 2** is downgraded to QoS 1 for subscriptions and for MQTT 3.1.1 publishes. An
  MQTT 5 client that publishes with QoS 2 is disconnected. Anonymous MQTT is off.
- **AMQP 1.0 shovels** (`amqp10` endpoints) are rejected; use `amqp091`.
- **TLS.** `rolld` serves plain TCP. Use your hosted endpoint, or terminate TLS in front of
  `rolld`, for `amqps`.
- **Stream offsets** change. Messages copied into a roll stream get new offsets. Stream
  consumers that store numeric offsets should restart from `first`, `last` or a timestamp.
- **Delayed messages still waiting on the source** aren't in a queue yet, so shovels don't
  see them. Let them fire before you decommission the source.
- **Delivery counts reset.** Messages moved by a shovel are new messages on roll, so their
  `x-delivery-count` starts again.

## Verification checklist

- [ ] Object counts match. Compare `roll queues ls`, `roll queues exchanges` and
      `roll queues bindings` with the source, or diff the exports:
      `roll queues definitions export roll.json` and `rabbitmqadmin export source.json`.
- [ ] Users can log in. Connect each application user once, or read a queue it can read
      over HTTP: `curl -u app:secret http://127.0.0.1:55680/v1/vhosts/%2F/queues/<queue>`.
- [ ] Every migration shovel shows `running` with `remaining: 0`
      (`roll --json queues migrate`).
- [ ] Consumers are on roll: `roll queues connections` lists them, and
      `roll queues show <queue>` shows the consumer count.
- [ ] The source has no clients: `curl -u admin:... $SRC/api/connections` returns `[]`.
- [ ] Round trip: `roll queues publish -k <queue> 'ping'`, then check that the consumer
      receives it, or `roll queues peek <queue>`.
- [ ] Dead-lettering works. Reject a test message and check that it reaches the DLQ
      (`roll queues peek <dlq>`).
- [ ] Monitoring scrapes `http://127.0.0.1:55673/metrics` (or `<your-endpoint>`), and
      dashboards show `rabbitmq_queue_messages_ready` from `/metrics/per-object`.
- [ ] Migration shovels and federation upstreams are deleted.
- [ ] Cost: compare the workload under **Queues → Usage** in the dashboard, or
      `roll queues usage`.
