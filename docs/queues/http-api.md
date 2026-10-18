# roll queues — HTTP messaging API

The HTTP API lets serverless functions, edge workers, browsers and scripts publish and
consume without holding an AMQP connection open. It runs on the same queues as AMQP, MQTT,
STOMP and the stream protocol, so you can publish over HTTP and consume with any RabbitMQ
client, or the other way round.

- Base URL: `http://127.0.0.1:55680` with `rolld` (port 443 behind TLS in production).
- Everything is under `/v1` and speaks JSON (`Content-Type: application/json`).
- `{vhost}` and `{queue}` are path segments, URL-encoded: the default vhost `/` is `%2F`.
- CORS is open (any origin, method and header), so browsers can call it directly.

The examples use the default `guest` / `guest` user and a queue called `orders`.

```sh
export ROLL=http://127.0.0.1:55680
```

## Contents

- [Authentication](#authentication)
- [Errors](#errors)
- [Queues](#queues)
- [Exchanges and bindings](#exchanges-and-bindings)
- [Publishing](#publishing)
- [Receiving (pull)](#receiving-pull)
- [Server-Sent Events](#server-sent-events)
- [WebSocket](#websocket)
- [Webhooks](#webhooks)
- [Tokens](#tokens)
- [Message format](#message-format)
- [Working with AMQP consumers](#working-with-amqp-consumers)
- [Health and shutdown](#health-and-shutdown)
- [Limits](#limits)

## Authentication

Every endpoint except `/v1/health` needs credentials. There are two ways to authenticate.

### HTTP Basic (broker users)

Use a broker user's name and password. Permissions are the same as over AMQP:

| operation                                           | AMQP permission                 |
| --------------------------------------------------- | ------------------------------- |
| publish                                             | `write` on the exchange         |
| receive, ack/nack, SSE, WebSocket, queue depth      | `read` on the queue             |
| list queues                                         | `read` or `configure` on each queue listed |
| declare or delete a queue                           | `configure` on the queue        |
| purge a queue                                       | `read` on the queue             |
| declare or delete an exchange                       | `configure` on the exchange (and `read` on its `alternate-exchange`) |
| bind or unbind                                      | `write` on the destination and `read` on the source exchange |
| manage webhooks                                     | `configure` and `read` on the queue |

These are the checks AMQP's `queue.delete`, `queue.purge`, `exchange.declare`, `queue.bind`
and so on make.

Publishing to the default exchange is checked against the name `amq.default`, as RabbitMQ
does. Topic permissions apply to topic exchanges.

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/queues/orders
```

### Bearer tokens

Scoped tokens are for code you don't want to give a password to, like a browser app or an
edge function. A token belongs to a user, is valid in one vhost, and can only do what its
scopes allow *and* what its user is allowed to do.

```sh
curl -H "Authorization: Bearer roll_3f9c0a1b2c3d4e5f_9xQ…" $ROLL/v1/vhosts/%2F/queues/orders
```

Browsers can't set headers on `EventSource` or `WebSocket`, so the token is also accepted as
an `access_token` query parameter:

```js
new EventSource(`${ROLL}/v1/vhosts/%2F/queues/orders/events?access_token=${token}`)
```

See [Tokens](#tokens) for creating them.

## Errors

Errors have an HTTP status and a JSON body:

```json
{ "error": { "code": "not_found", "message": "NOT_FOUND - no queue 'orders' in vhost '/'" } }
```

When the broker is the source of the error, the message is RabbitMQ's text, as an AMQP client
would show it.

| status | code                                 | when                                                      |
| ------ | ------------------------------------ | --------------------------------------------------------- |
| 400    | `bad_request`                        | invalid JSON, a missing field, or a value out of range    |
| 400    | `invalid_receipt`                    | the receipt is forged or garbled, or is for another queue |
| 401    | `unauthorized`                       | no credentials, or wrong, revoked or expired ones         |
| 403    | `access_refused`                     | no permission or token scope for the resource             |
| 404    | `not_found`                          | no such vhost, queue, exchange, webhook, token or endpoint |
| 409    | `precondition_failed`                | an inequivalent redeclare, `if_empty` / `if_unused` not met, a delivery-ack timeout |
| 409    | `resource_locked`                    | the queue is exclusive to an AMQP connection              |
| 409    | `receipt_stale`                      | the message was already acked, returned or redelivered    |
| 409    | `idempotency_in_progress`            | the same `Idempotency-Key` is still being processed       |
| 410    | `receipt_expired`                    | the visibility timeout passed; the message went back to the queue |
| 410    | `session_closed`                     | the SSE stream that delivered the message has ended       |
| 413    | `content_too_large`                  | a message body is over the broker's maximum (16 MiB by default) |
| 422    | `idempotency_key_reused`             | the `Idempotency-Key` was used with a different body      |
| 503    | `unavailable`                        | the broker is draining, or publishing is blocked by an alarm |

## Queues

### Declare a queue

`PUT /v1/vhosts/{vhost}/queues/{queue}`

| field         | default   |                                              |
| ------------- | --------- | -------------------------------------------- |
| `type`        | vhost default | `classic`, `quorum` or `stream`          |
| `durable`     | `true`    |                                              |
| `auto_delete` | `false`   |                                              |
| `arguments`   | `{}`      | any `x-` queue argument RabbitMQ accepts     |

Declaring is idempotent. The response is `201` when the queue is created and `200` when it
already exists. An inequivalent redeclare is a `409`, as in AMQP.

```sh
curl -u guest:guest -X PUT $ROLL/v1/vhosts/%2F/queues/orders \
  -d '{"type": "quorum", "arguments": {"x-dead-letter-exchange": "", "x-dead-letter-routing-key": "orders.dlq"}}'
```

### Queue depth

`GET /v1/vhosts/{vhost}/queues/{queue}`

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/queues/orders
```

```json
{
  "name": "orders", "vhost": "/", "type": "quorum", "durable": true, "auto_delete": false, "exclusive": false,
  "arguments": {"x-queue-type": "quorum"},
  "messages_ready": 12, "messages_unacked": 3, "messages": 15, "consumers": 1
}
```

Stream queues also report `first_offset` and `next_offset`.

### List queues

`GET /v1/vhosts/{vhost}/queues`

Returns the queues of a vhost the caller may read or configure, sorted by name, each in the
same shape as [queue depth](#queue-depth). `?prefix=orders` keeps only names that start with
`orders`. Exclusive queues (owned by an AMQP connection) aren't listed.

```sh
curl -u guest:guest "$ROLL/v1/vhosts/%2F/queues?prefix=orders"
```

```json
[
  { "name": "orders", "vhost": "/", "type": "quorum", "messages_ready": 12, "messages_unacked": 3, "messages": 15, "consumers": 1, … },
  { "name": "orders.dlq", "vhost": "/", "type": "quorum", "messages_ready": 0, "messages_unacked": 0, "messages": 0, "consumers": 0, … }
]
```

### Purge a queue

`POST /v1/vhosts/{vhost}/queues/{queue}/purge`

Drops the ready messages. Messages leased or delivered to consumers aren't touched; they come
back if they are returned.

```sh
curl -u guest:guest -X POST $ROLL/v1/vhosts/%2F/queues/orders/purge
```

```json
{ "purged": 12 }
```

### Delete a queue

`DELETE /v1/vhosts/{vhost}/queues/{queue}`

Deletes the queue, its messages and its bindings. Options, as query parameters:

| parameter   |                                                         |
| ----------- | ------------------------------------------------------- |
| `if_empty`  | `true`: only delete it if it has no messages (else `409`) |
| `if_unused` | `true`: only delete it if it has no consumers (else `409`) |

```sh
curl -u guest:guest -X DELETE "$ROLL/v1/vhosts/%2F/queues/orders?if_empty=true"
```

```json
{ "deleted": "orders", "messages": 0 }
```

`messages` is how many messages went with it. A queue that doesn't exist is a `404`, and a
queue exclusive to an AMQP connection is a `409` (`resource_locked`).

## Exchanges and bindings

### Declare an exchange

`PUT /v1/vhosts/{vhost}/exchanges/{exchange}`

| field         | default |                                                              |
| ------------- | ------- | ------------------------------------------------------------ |
| `type`        | required | `direct`, `fanout`, `topic`, `headers`, `x-delayed-message`, … |
| `durable`     | `true`  |                                                              |
| `auto_delete` | `false` |                                                              |
| `internal`    | `false` | internal exchanges only receive from other exchanges         |
| `arguments`   | `{}`    | e.g. `alternate-exchange`, or `x-delayed-type` for `x-delayed-message` |

Like queues, declaring is idempotent: `201` when created, `200` when it already exists, and
`409` for an inequivalent redeclare. Names starting with `amq.` are reserved (`403`).

```sh
curl -u guest:guest -X PUT $ROLL/v1/vhosts/%2F/exchanges/orders.events -d '{"type": "topic"}'
```

```json
{ "name": "orders.events", "vhost": "/", "type": "topic", "durable": true, "auto_delete": false, "internal": false, "arguments": {} }
```

### Delete an exchange

`DELETE /v1/vhosts/{vhost}/exchanges/{exchange}` answers `204`. With `?if_unused=true` it is
only deleted if nothing is bound to it (else `409`). An unknown exchange is a `404`, and the
`amq.*` exchanges can't be deleted (`403`).

### Bind and unbind

`POST /v1/vhosts/{vhost}/bindings` binds a queue (or an exchange) to an exchange, and
`DELETE /v1/vhosts/{vhost}/bindings` with the same body removes the binding.

| field              | default   |                                                  |
| ------------------ | --------- | ------------------------------------------------ |
| `source`           | required  | the exchange messages come from                  |
| `destination`      | required  | the queue (or exchange) they go to               |
| `destination_type` | `"queue"` | `"queue"` or `"exchange"`                        |
| `routing_key`      | `""`      | the binding key, e.g. `order.#` for a topic exchange |
| `arguments`        | `{}`      | e.g. `x-match` for a headers exchange             |

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/bindings \
  -d '{"source": "orders.events", "destination": "orders", "routing_key": "order.#"}'
```

Binding answers `201` with the binding, and is a no-op if it already exists. Unbinding answers
`204`, also when there was no such binding. A missing source, destination or exchange is a
`404`. The default exchange (`amq.default`) can't be bound to.

This is how SNS-style fan-out works: declare a `fanout` or `topic` exchange, bind each
subscriber's queue to it, and publish to the exchange.

## Publishing

There are two endpoints:

- `POST /v1/vhosts/{vhost}/exchanges/{exchange}/publish` publishes through an exchange. Use
  `amq.default` for the default exchange.
- `POST /v1/vhosts/{vhost}/queues/{queue}/messages` publishes straight to a queue, through
  the default exchange. It gives `404` if the queue doesn't exist.

Each takes one message:

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/queues/orders/messages \
  -d '{"body": {"order": 1042, "total": 99.5}}'
```

```json
{ "id": "8f1d0c5e2b7a4c39a1e0d6f3b2c49e17", "routed": true, "confirmed": true }
```

or a batch of up to 500, published in order:

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/exchanges/amq.topic/publish -d '{
  "messages": [
    {"routing_key": "eu.orders.created", "body": "first"},
    {"routing_key": "us.orders.created", "body": "second", "properties": {"priority": 5}}
  ]
}'
```

```json
{ "results": [
  { "id": "…", "routed": true, "confirmed": true },
  { "id": "…", "routed": true, "confirmed": true }
] }
```

### Message fields

| field           | default            |                                                              |
| --------------- | ------------------ | ------------------------------------------------------------ |
| `routing_key`   | `""`               | on the queue endpoint it is the queue name (it may be omitted) |
| `body`          | empty              | see `body_encoding`                                          |
| `body_encoding` | from the body type | `utf8` (a string), `base64` (binary as a base64 string) or `json` (any JSON value) |
| `properties`    | `{}`               | AMQP properties: `content_type`, `content_encoding`, `delivery_mode`, `priority`, `correlation_id`, `reply_to`, `expiration`, `message_id`, `timestamp`, `type`, `user_id`, `app_id`, `headers` |
| `headers`       | `{}`               | message headers, merged into `properties.headers`            |
| `id`            | generated          | becomes `message_id`, and is deduplicated (see below)        |
| `mandatory`     | `false`            | count an unroutable message as returned, as AMQP does         |

The defaults are chosen for HTTP:

- Messages are persistent (`delivery_mode: 2`) unless you set `delivery_mode: 1`.
- A body that is a JSON object, array, number or boolean is sent as JSON with
  `content_type: application/json`.
- Every message gets a `message_id`: your `id`, your `properties.message_id`, or a
  generated one. It is returned as `id`.
- `expiration` must be a non-negative integer of milliseconds, as a string or a number.
- `user_id` must match the authenticated user, unless that user has the `impersonator` tag.

### Confirms

The response is sent only after the broker has the messages durably, in the same way as
AMQP publisher confirms.

- `routed`: at least one queue received the message. Unroutable messages are dropped, or go
  to the exchange's alternate exchange.
- `confirmed`: every queue that received it has stored it. A message that is unroutable is
  still `confirmed: true`, because there was nothing to store. `confirmed: false` means the
  broker refused the message, for example because a queue with `x-overflow: reject-publish`
  is full or storage failed. Retry it.

### Idempotency-Key

Send an `Idempotency-Key` header to make a request safe to retry. The first successful
response is stored for 24 hours. Repeating the request with the same key returns the stored
response, with the header `Idempotent-Replayed: true`, and publishes nothing.

- Keys are scoped to the user (or token) and the endpoint.
- Using a key again with a different body gives `422`.
- Using it while the first request is still running gives `409`.
- Failed requests aren't stored, so you can fix the request and retry with the same key.

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/queues/orders/messages \
  -H "Idempotency-Key: checkout-7f3a" -d '{"body": "charge 1042"}'
```

### Message id deduplication

A message published with an explicit `id` (or `properties.message_id`) within the last 5
minutes, to the same exchange with the same routing key, isn't published again. You get the
first result back with `"duplicate": true`. This covers retries that happen without an
`Idempotency-Key`.

If the queue has `x-message-deduplication: true`, or the exchange is an
`x-message-deduplication` exchange, the id is also set as the `x-deduplication-header` header,
so the broker's own deduplication applies to it as well.

## Receiving (pull)

`POST /v1/vhosts/{vhost}/queues/{queue}/receive`

| field           | default  |                                                       |
| --------------- | -------- | ----------------------------------------------------- |
| `max`           | 1        | 1–100 messages                                        |
| `wait_ms`       | 0        | long-poll up to 20000 ms when the queue is empty      |
| `visibility_ms` | 30000    | how long the messages stay invisible (up to 12 hours) |

Receiving leases messages. They stay invisible to other consumers (HTTP or AMQP) for
`visibility_ms`. Settle each one with its `receipt` before then. If you don't, it goes back to
the queue and is redelivered with `redelivered: true` and its `delivery_count` increased by
one.

A long-poll returns as soon as a message arrives, so `wait_ms: 20000` in a loop is a cheap way
to consume with low latency.

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/queues/orders/receive \
  -d '{"max": 10, "wait_ms": 20000, "visibility_ms": 60000}'
```

```json
{ "messages": [ {
  "id": "8f1d0c5e2b7a4c39a1e0d6f3b2c49e17",
  "queue": "orders", "exchange": "", "routing_key": "orders",
  "body": "{\"order\":1042,\"total\":99.5}", "body_encoding": "utf8",
  "properties": {"content_type": "application/json", "delivery_mode": 2, "message_id": "8f1d0c5e2b7a4c39a1e0d6f3b2c49e17"},
  "headers": {},
  "redelivered": false, "delivery_count": 0, "published_at_ms": 1760700000000,
  "visible_at_ms": 1760700060000,
  "receipt": "AQEAAAAAAAAAAgAAAAAAAAGZ…"
} ] }
```

Receipts are opaque, signed strings that name the queue, the message and the lease. Every
broker that shares the metadata store accepts them. Stream queues can't be leased; read them
with [Server-Sent Events](#server-sent-events) or the [WebSocket](#websocket) instead.

### Acknowledge

`POST /v1/vhosts/{vhost}/queues/{queue}/ack` with `{"receipts": [...]}` removes the messages.

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/queues/orders/ack -d '{"receipts": ["AQEAAAAAAAAAAgAAAAAAAAGZ…"]}'
```

```json
{ "succeeded": 1, "results": [ { "receipt": "AQEAAAAAAAAAAgAAAAAAAAGZ…", "ok": true } ] }
```

### Negative acknowledge

`POST /v1/vhosts/{vhost}/queues/{queue}/nack` takes these fields:

| field      | default |                                                                         |
| ---------- | ------- | ----------------------------------------------------------------------- |
| `receipts` |         |                                                                         |
| `requeue`  | `true`  | `true` returns the messages to the queue. `false` rejects them: they are dead-lettered (reason `rejected`) if the queue has a dead-letter exchange, and dropped otherwise. |
| `delay_ms` | 0       | with `requeue`, keep the messages invisible for this long before they return. This is a retry backoff. |

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/queues/orders/nack \
  -d '{"receipts": ["AQEAAAAAAAAAAgAAAAAAAAGZ…"], "requeue": true, "delay_ms": 5000}'
```

### Change visibility

`POST /v1/vhosts/{vhost}/queues/{queue}/visibility` with `{"receipts": [...], "visibility_ms": N}`.

- It sets how much longer the messages stay invisible, counted from now. Use it to keep
  working on a slow message.
- `0` returns the messages to the queue right away.
- The response has a fresh receipt for each message, carrying the new deadline. The old
  receipts keep working.

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/queues/orders/visibility \
  -d '{"receipts": ["AQEAAAAAAAAAAgAAAAAAAAGZ…"], "visibility_ms": 120000}'
```

### Settlement results

The settle endpoints take up to 1000 receipts and process each one.

- If every receipt succeeds, the response is `200` with `succeeded` and a per-receipt
  `results` list.
- If any receipt fails, the status and `error` are those of the first failure. `results`
  still shows which receipts succeeded.

| status | code              | meaning                                                                       |
| ------ | ----------------- | ----------------------------------------------------------------------------- |
| 400    | `invalid_receipt` | not a receipt from this broker, or it is for another queue                    |
| 410    | `receipt_expired` | the visibility timeout passed and the message went back to the queue. Receive it again. |
| 409    | `receipt_stale`   | the message was already acknowledged, returned or redelivered                 |
| 410    | `session_closed`  | (SSE receipts) the stream ended, so its messages went back to the queue       |

## Server-Sent Events

`GET /v1/vhosts/{vhost}/queues/{queue}/events?ack=auto|manual&prefetch=N`

This streams messages as they arrive. Every event is a JSON object.

| event       | data                                                                    |
| ----------- | ----------------------------------------------------------------------- |
| `open`      | `{consumer_tag, queue, prefetch, ack}`: the consumer is subscribed      |
| `message`   | a [message](#message-format). In `manual` mode it also has a `receipt`. The event `id` is the message's position (the offset, for streams). |
| `cancelled` | the queue was deleted. The stream ends.                                  |
| `close`     | `{reason}`: the connection was closed from the management API, or the broker is shutting down. Reconnect. |
| `error`     | `{code, message}`: for example, a delivery wasn't acknowledged within the consumer timeout |

A `: heartbeat` comment is sent every 15 seconds to keep proxies from timing out.

| parameter      | default                     |                                                        |
| -------------- | --------------------------- | ------------------------------------------------------ |
| `ack`          | `auto`                      | `auto` acknowledges each message as it is written to the stream (at-most-once if the connection drops). `manual` sends a receipt that you settle with the [`ack`/`nack` endpoints](#acknowledge). |
| `prefetch`     | 100 (`auto`), 10 (`manual`) | 1–10000 messages in flight (unacknowledged) at once     |
| `offset`       | `next`                      | stream queues: `first`, `last`, `next`, an offset number, or an interval like `1h` |
| `priority`     | 0                           | consumer priority (`x-priority`)                       |
| `access_token` |                             | a bearer token, for browsers                           |

In `manual` mode:

- You can't use `delay_ms` or `visibility` with receipts from a stream.
- When the stream ends, every message it hasn't acknowledged goes back to the queue.
- Receipts are only valid on the broker that served the stream.

On a stream queue, reconnecting with `Last-Event-ID` resumes after that offset. Browsers'
`EventSource` sends this header automatically.

```sh
curl -N -u guest:guest "$ROLL/v1/vhosts/%2F/queues/orders/events?ack=manual&prefetch=20"
```

```
event: open
data: {"ack":"manual","consumer_tag":"http.ctag-1f","prefetch":20,"queue":"orders"}

id: 17
event: message
data: {"body":"charge 1042","delivery_count":0,"id":"…","receipt":"AQIAAAAAAAAAEQAA…", …}

: heartbeat
```

```js
const es = new EventSource(`${ROLL}/v1/vhosts/%2F/queues/orders/events?access_token=${token}`)
es.addEventListener('message', (e) => handle(JSON.parse(e.data)))
```

## WebSocket

`GET /v1/vhosts/{vhost}/queues/{queue}/ws?ack=auto|manual&prefetch=N`

This is a two-way consumer (the same query parameters as SSE), and it can also publish. Every
frame is a JSON text frame with a `type`.

Server to client:

| type        | fields                                                                 |
| ----------- | ---------------------------------------------------------------------- |
| `open`      | `consumer: {consumer_tag, queue, prefetch, ack}`                        |
| `deliver`   | `delivery_tag` and `message`. Tags count up from 1 for each connection. |
| `confirm`   | the reply to a `publish`: `{ref, id, routed, confirmed}`               |
| `error`     | `{ref?, error: {code, message}}`. The connection stays open.           |
| `cancelled` | the queue was deleted. The server closes the socket.                   |

Client to server:

| type      | fields                                                                                   |
| --------- | ---------------------------------------------------------------------------------------- |
| `ack`     | `delivery_tag` and `multiple` (default `false`)                                          |
| `nack`    | `delivery_tag`, `multiple` (default `false`) and `requeue` (default `true`; `false` dead-letters) |
| `credit`  | `prefetch`: change how many unacknowledged deliveries may be in flight                   |
| `publish` | a [message](#message-fields) plus `exchange` (default `""`) and `ref` (any JSON value, echoed in the `confirm`). On the default exchange, `routing_key` defaults to this queue. |

- Publishes from one socket reach the broker in the order they were sent.
- The server pings every 15 seconds.
- When the socket closes, every message it hasn't acknowledged goes back to the queue.

```js
const ws = new WebSocket(`${ROLL.replace('http', 'ws')}/v1/vhosts/%2F/queues/orders/ws?ack=manual&access_token=${token}`)
ws.onmessage = (e) => {
  const f = JSON.parse(e.data)
  if (f.type === 'deliver') {
    handle(f.message)
    ws.send(JSON.stringify({ type: 'ack', delivery_tag: f.delivery_tag }))
  }
}
ws.onopen = () => ws.send(JSON.stringify({ type: 'publish', ref: 1, body: { order: 1043 } }))
```

## Webhooks

A webhook consumes a queue and POSTs each message to your URL.

- A `2xx` response acknowledges the message.
- Anything else, or a timeout, is retried with exponential backoff and jitter.
- After the last attempt the message is rejected. It is dead-lettered if the queue has a
  dead-letter exchange, and dropped otherwise.

Webhooks are stored in the metadata store and restart with the broker. In a cluster, every node
runs every webhook as competing consumers, and a webhook created, changed or deleted through any
node takes effect on all of them within the change-feed interval. `status` counts deliveries made
by the node that answers the request.

### Create or replace

`PUT /v1/vhosts/{vhost}/webhooks/{name}`

| field                  | default   |                                                   |
| ---------------------- | --------- | ------------------------------------------------- |
| `queue`                |           | the queue to consume (it may be declared later)   |
| `url`                  |           | `http` or `https`                                 |
| `secret`               | generated | the signing key (`whsec_…`). Updating a webhook keeps its secret unless you send a new one. |
| `max_in_flight`        | 10        | concurrent deliveries (1–1000)                    |
| `timeout_ms`           | 10000     | per-attempt timeout (100–300000)                  |
| `retry.max_attempts`   | 5         | attempts per message (1–100)                      |
| `retry.backoff_ms`     | 1000      | delay before the second attempt; doubles each time |
| `retry.max_backoff_ms` | 60000     | cap on the delay                                  |
| `headers`              | `{}`      | extra request headers (not `Roll-*` or `Content-Type`) |

The response is `201` (created) or `200` (replaced). It is the only response that includes the
`secret`.

```sh
curl -u guest:guest -X PUT $ROLL/v1/vhosts/%2F/webhooks/orders-to-billing -d '{
  "queue": "orders",
  "url": "https://billing.example.com/hooks/orders",
  "max_in_flight": 20,
  "retry": {"max_attempts": 8, "backoff_ms": 2000, "max_backoff_ms": 300000},
  "headers": {"X-Team": "billing"}
}'
```

### Status, list and delete

| request                                            |                                                    |
| -------------------------------------------------- | -------------------------------------------------- |
| `GET /v1/vhosts/{vhost}/webhooks/{name}`           | the definition and its `status`                    |
| `GET /v1/vhosts/{vhost}/webhooks`                  | every webhook on queues you can read               |
| `DELETE /v1/vhosts/{vhost}/webhooks/{name}`        | stop it; messages in flight go back to the queue   |

`status` has these fields:

- `state`: `running`, `waiting_for_queue`, `waiting_for_vhost`, `unauthorized` (the owner
  lost read permission on the queue), `starting` or `stopped`.
- `delivered`, `failed` (rejected after the last attempt) and `retries`.
- `in_flight`.
- `last_success_at_ms`, `last_error` and `last_error_at_ms`.

```json
{
  "name": "orders-to-billing", "vhost": "/", "queue": "orders", "url": "https://billing.example.com/hooks/orders",
  "max_in_flight": 20, "timeout_ms": 10000, "retry": {"max_attempts": 8, "backoff_ms": 2000, "max_backoff_ms": 300000},
  "headers": {"X-Team": "billing"}, "owner": "guest", "created_at": 1760700000, "updated_at": 1760700000,
  "status": {"state": "running", "delivered": 1290, "failed": 2, "retries": 17, "in_flight": 3,
             "last_success_at_ms": 1760703600123, "last_error": "HTTP 503 Service Unavailable", "last_error_at_ms": 1760703512000}
}
```

### The request

`POST <url>`. The body is the [message](#message-format) JSON plus `vhost` and `webhook`.

| header                  |                                                              |
| ----------------------- | ------------------------------------------------------------ |
| `Content-Type`          | `application/json`                                           |
| `Roll-Signature`        | `t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>` |
| `Roll-Webhook-Id`       | the webhook's name                                           |
| `Roll-Message-Id`       | the message id, the same on every attempt. Use it to deduplicate. |
| `Roll-Delivery-Attempt` | 1, 2, … within this delivery                                 |

Delivery is at least once. A message can come again after a timeout, a broker restart or a
webhook update, so make your handler idempotent on `Roll-Message-Id`.

To verify a request, compute the HMAC over the raw body, compare it in constant time, and
reject old timestamps:

```js
import crypto from 'node:crypto'

function verify(secret, header, rawBody, toleranceS = 300) {
  const parts = Object.fromEntries(header.split(',').map((kv) => kv.split('=')))
  const expected = crypto.createHmac('sha256', secret).update(`${parts.t}.${rawBody}`).digest('hex')
  const fresh = Math.abs(Date.now() / 1000 - Number(parts.t)) <= toleranceS
  return fresh && crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(parts.v1))
}
```

```python
import hashlib, hmac, time

def verify(secret: str, header: str, raw_body: bytes, tolerance_s: int = 300) -> bool:
    parts = dict(kv.split("=", 1) for kv in header.split(","))
    expected = hmac.new(secret.encode(), f"{parts['t']}.".encode() + raw_body, hashlib.sha256).hexdigest()
    return abs(time.time() - int(parts["t"])) <= tolerance_s and hmac.compare_digest(expected, parts["v1"])
```

## Tokens

Only users (with HTTP Basic) can manage tokens; a token can't create other tokens.

- Administrators see and manage every token.
- Other users see and manage their own.

### Create

`POST /v1/tokens`

| field        |                                                                       |
| ------------ | --------------------------------------------------------------------- |
| `name`       | a label                                                               |
| `vhost`      | the one vhost the token works in                                      |
| `scopes`     | a list of `publish:<exchange-regex>`, `consume:<queue-regex>` and `manage:<name-regex>` |
| `expires_in` | optional, in seconds                                                  |
| `user`       | optional, administrators only: create the token for another user      |

What each scope covers:

- `publish` covers the publish endpoints and WebSocket publishes.
- `consume` covers receive, ack/nack/visibility, SSE, the WebSocket and reading a queue's depth.
- `manage` covers declaring, purging and deleting queues, declaring and deleting exchanges
  (the pattern matches the exchange name), binding and unbinding (it matches the
  destination), and, together with `consume`, managing webhooks.
- Listing queues shows the queues a `consume` or `manage` scope matches.

The token's user still needs the AMQP permissions in the table under
[HTTP Basic](#http-basic-broker-users).

The patterns are regular expressions, unanchored as in RabbitMQ permissions, so write
`^orders$` to match exactly. For the default exchange, a `publish` scope may match either
`amq.default` (any queue) or the queue name.

```sh
curl -u guest:guest $ROLL/v1/tokens -d '{
  "name": "storefront",
  "vhost": "/",
  "scopes": ["publish:^orders$", "consume:^notifications\\."],
  "expires_in": 2592000
}'
```

```json
{
  "id": "3f9c0a1b2c3d4e5f", "name": "storefront", "user": "guest", "vhost": "/",
  "scopes": ["publish:^orders$", "consume:^notifications\\."],
  "created_at": 1760700000, "expires_at": 1763292000,
  "token": "roll_3f9c0a1b2c3d4e5f_9xQ…"
}
```

The `token` is shown only once. The broker stores only a hash of it.

### List, show and revoke

| request                     |                              |
| --------------------------- | ---------------------------- |
| `GET /v1/tokens`            | list tokens (never their secrets) |
| `GET /v1/tokens/{id}`       | show one token               |
| `DELETE /v1/tokens/{id}`    | revoke it                    |

```sh
curl -u guest:guest -X DELETE $ROLL/v1/tokens/3f9c0a1b2c3d4e5f
```

A revoked token stops working within about 10 seconds on every broker. A token also stops
working when its user is deleted or loses the permission.

## Message format

Messages from `receive`, SSE, the WebSocket and webhooks look like this:

| field             |                                                                                   |
| ----------------- | --------------------------------------------------------------------------------- |
| `id`              | `message_id`, or `null` (AMQP publishers may not set one)                         |
| `queue`           | the queue it came from                                                            |
| `exchange`        | the exchange it was published to                                                  |
| `routing_key`     | its routing key                                                                   |
| `body`            | the body as a string                                                              |
| `body_encoding`   | `utf8` for text; `base64` for binary (not valid UTF-8, or containing control characters other than tab and newlines) |
| `properties`      | AMQP properties, without headers                                                  |
| `headers`         | message headers. They include `x-death` after dead-lettering, `x-delivery-count` on quorum-queue redeliveries and `x-stream-offset` on streams. |
| `redelivered`     | whether it was delivered before                                                   |
| `delivery_count`  | how many times it was delivered before this time                                  |
| `published_at_ms` | when the broker received it                                                       |
| `offset`          | streams only                                                                      |
| `receipt`         | pull and manual SSE only                                                          |
| `visible_at_ms`   | pull only: when the lease runs out                                                |

## Working with AMQP consumers

HTTP and AMQP consumers can share a queue.

- A message leased by `receive` is invisible to AMQP consumers. If the lease runs out, the
  message is redelivered to the next consumer, AMQP or HTTP, with `redelivered` set.
- Quorum queues count every return, from an expired lease, a nack or a closed stream, in
  `x-delivery-count`. A queue's `x-delivery-limit` applies to HTTP consumers as well:
  messages that hit the limit are dead-lettered with reason `delivery_limit`.
- `nack` with `requeue: false`, webhooks that run out of attempts, and WebSocket nacks all
  reject, which dead-letters through the queue's `x-dead-letter-exchange` with an `x-death`
  entry, as in RabbitMQ.
- SSE and WebSocket consumers are listed with the queue's consumers. Their connections
  (protocol `HTTP SSE` or `WebSocket`) appear in the management API and can be force-closed
  from it.

## Health and shutdown

`GET /v1/health` needs no authentication. It returns
`{"status": "ok", "node": "...", "version": "...", "url": ...}`, or `503` with
`"status": "draining"` while the broker shuts down.

When the broker drains, for a restart or an upgrade:

- Long-polls return at once with what they have.
- SSE streams get a `close` event and WebSockets a close frame (1001). Their unacknowledged
  messages go back to the queue.
- Webhooks stop, returning their in-flight messages.

Clients should reconnect, and the load balancer will pick another broker.

## Limits

| limit                                 | value                              |
| ------------------------------------- | ---------------------------------- |
| messages per publish batch            | 500                                |
| message body                          | the broker's maximum (16 MiB)      |
| `receive` `max`                       | 100                                |
| long-poll `wait_ms`                   | 20000                              |
| `visibility_ms`, `delay_ms`           | 12 hours                           |
| receipts per settle request           | 1000                               |
| SSE and WebSocket `prefetch`          | 10000                              |
| `Idempotency-Key` lifetime            | 24 hours (up to 100000 stored)     |
| message id dedup window               | 5 minutes                          |
