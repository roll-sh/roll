# Migrate from Amazon SQS

roll queues doesn't speak the SQS API, so the AWS SDKs can't point at it. Its
[HTTP API](http-api.md) has the same shape, though. You receive messages with a long-poll,
they stay invisible for a visibility timeout, and you delete them with a receipt. Most SQS code
moves by swapping SDK calls for plain HTTP requests. There is no automatic SQS importer. Move
traffic with the [dual-run strategy](#migration-strategy) below.

```sh
export ROLL=http://127.0.0.1:55680        # HTTP API. Hosted: https://<your-endpoint>
```

Paths name a vhost and a queue: `/v1/vhosts/{vhost}/queues/{queue}`. The default vhost `/` is
`%2F`. The examples use the default `guest` / `guest` user and a queue called `orders`.

## Concept mapping

| SQS                                          | roll queues                                                              |
| -------------------------------------------- | ------------------------------------------------------------------------ |
| queue (queue URL)                            | queue: `$ROLL/v1/vhosts/%2F/queues/orders`                               |
| `CreateQueue`                                | `PUT /v1/vhosts/{vhost}/queues/{queue}`, which is idempotent             |
| `GetQueueAttributes` (approximate counts)    | `GET /v1/vhosts/{vhost}/queues/{queue}`: `messages_ready`, `messages_unacked`, `consumers` |
| `SendMessage`                                | `POST …/queues/{queue}/messages` with `{"body": …}`                      |
| `SendMessageBatch` (10)                      | the same endpoint with `{"messages": [...]}`, up to 500, published in order |
| `MessageAttributes`                          | `headers` (any JSON value), or AMQP `properties`                         |
| `ReceiveMessage` with `WaitTimeSeconds` (≤ 20 s) | `POST …/receive` with `wait_ms` (≤ 20000)                            |
| `MaxNumberOfMessages` (≤ 10)                 | `max` (≤ 100)                                                            |
| `VisibilityTimeout` (≤ 12 h)                 | `visibility_ms` on `receive` (default 30000, ≤ 12 h)                     |
| `ChangeMessageVisibility`                    | `POST …/visibility` with `{"receipts": [...], "visibility_ms": N}`; `0` returns the message now |
| `ReceiptHandle`                              | `receipt`                                                                |
| `DeleteMessage` / `DeleteMessageBatch`       | `POST …/ack` with `{"receipts": [...]}`, up to 1000                      |
| "don't delete, let it time out" (retry)      | `POST …/nack` with `requeue: true` and an optional `delay_ms` backoff    |
| `ApproximateReceiveCount`                    | `delivery_count + 1` (`delivery_count` counts earlier deliveries)        |
| `SentTimestamp`                              | `published_at_ms`                                                        |
| redrive policy, `maxReceiveCount`            | quorum queue with `x-dead-letter-exchange` / `x-dead-letter-routing-key` and `x-delivery-limit` |
| DLQ redrive (`StartMessageMoveTask`)         | `roll queues replay <dlq>`, or `POST /v1/queues/vhosts/{vhost}/queues/{dlq}/replay` on the roll API |
| `DelaySeconds` per message, message timers   | publish through an `x-delayed-message` exchange with an `x-delay` header (ms) |
| `MessageRetentionPeriod`                     | messages stay until consumed. Set `x-message-ttl` (ms) to expire them.   |
| FIFO queue, `MessageGroupId`                 | partial; see [FIFO](#fifo-queues)                                        |
| `MessageDeduplicationId`                     | the message `id` (5-minute window), or an `Idempotency-Key` header (24 h) |
| Lambda event source mapping                  | [webhooks](http-api.md#webhooks): one POST per message, a 2xx acknowledges |
| IAM policies                                 | broker users with permission regexes (HTTP Basic), or scoped [tokens](http-api.md#tokens) |
| SNS → SQS fan-out                            | a `fanout` or `topic` exchange bound to several queues: `PUT /v1/vhosts/{vhost}/exchanges/{exchange}` and `POST /v1/vhosts/{vhost}/bindings` |
| `ListQueues` (`QueueNamePrefix`)             | `GET /v1/vhosts/{vhost}/queues?prefix=…`                                 |
| `PurgeQueue`                                 | `POST …/queues/{queue}/purge`                                            |
| `DeleteQueue`                                | `DELETE …/queues/{queue}` (`?if_empty=true` to keep a queue that still has messages) |

## Create queues

A queue with a dead-letter queue, equivalent to an SQS queue with a redrive policy of
`maxReceiveCount: 5` and 4-day retention:

```sh
curl -u guest:guest -X PUT $ROLL/v1/vhosts/%2F/queues/orders.dlq -d '{"type": "quorum"}'

curl -u guest:guest -X PUT $ROLL/v1/vhosts/%2F/queues/orders -d '{
  "type": "quorum",
  "arguments": {
    "x-dead-letter-exchange": "",
    "x-dead-letter-routing-key": "orders.dlq",
    "x-delivery-limit": 4,
    "x-message-ttl": 345600000
  }
}'
```

- **Delivery limit.** A message is dead-lettered (reason `delivery_limit`) when it returns to
  the queue more than `x-delivery-limit` times, so it is delivered at most
  `x-delivery-limit + 1` times. For SQS `maxReceiveCount: M`, set `M - 1`. Expired
  visibility timeouts, `nack` with `requeue: true` and closed SSE streams all count as returns.
  Quorum queues default to a limit of 20.
- **`x-message-ttl`.** Expired messages are dead-lettered with reason `expired` when the queue
  has a dead-letter exchange, and dropped otherwise. SQS deletes them silently.
- **Re-drive.** `roll queues replay orders.dlq` publishes dead-lettered messages back to the
  exchange and routing key in their `x-death` history. `--count`, `--to-exchange` and
  `--to-key` override that.

## Code: boto3 → HTTP

### Before (boto3)

```python
import json, boto3

sqs = boto3.client("sqs")
URL = "https://sqs.us-east-1.amazonaws.com/123456789012/orders"

sqs.send_message(
    QueueUrl=URL,
    MessageBody=json.dumps({"order": 1042}),
    MessageAttributes={"tenant": {"DataType": "String", "StringValue": "acme"}},
)

while True:
    r = sqs.receive_message(QueueUrl=URL, MaxNumberOfMessages=10, WaitTimeSeconds=20, VisibilityTimeout=60)
    for m in r.get("Messages", []):
        handle(json.loads(m["Body"]))
        sqs.delete_message(QueueUrl=URL, ReceiptHandle=m["ReceiptHandle"])
```

### After (requests)

```python
import json, requests

ROLL = "http://127.0.0.1:55680"                   # hosted: https://<your-endpoint>
Q = f"{ROLL}/v1/vhosts/%2F/queues/orders"
s = requests.Session()
s.auth = ("guest", "guest")                        # or: s.headers["Authorization"] = f"Bearer {token}"

s.post(f"{Q}/messages", json={"body": {"order": 1042}, "headers": {"tenant": "acme"}}).raise_for_status()

while True:
    r = s.post(f"{Q}/receive", json={"max": 10, "wait_ms": 20000, "visibility_ms": 60000}, timeout=30)
    r.raise_for_status()
    for m in r.json()["messages"]:
        handle(json.loads(m["body"]))              # JSON bodies arrive as text, content_type application/json
        s.post(f"{Q}/ack", json={"receipts": [m["receipt"]]}).raise_for_status()
```

- A JSON object, array, number or boolean `body` is published as JSON with
  `content_type: application/json`. It comes back as a string in `body`. Binary bodies come
  back with `body_encoding: "base64"`.
- `MessageAttributes` become `headers`, and they arrive in the message's `headers`.
- Messages are persistent unless you set `properties.delivery_mode: 1`.
- The publish response arrives after the message is stored. `confirmed: false` means the
  broker refused it, so retry.

Ack in batches to save round trips (up to 1000 receipts per request):

```python
done = []
for m in r.json()["messages"]:
    handle(json.loads(m["body"]))
    done.append(m["receipt"])
if done:
    out = s.post(f"{Q}/ack", json={"receipts": done})
    # On any failure the status is that of the first failed receipt;
    # out.json()["results"] still lists which receipts succeeded.
```

### Visibility and retries

```python
# sqs.change_message_visibility(QueueUrl=URL, ReceiptHandle=h, VisibilityTimeout=120)
r = s.post(f"{Q}/visibility", json={"receipts": [receipt], "visibility_ms": 120000})
receipt = r.json()["results"][0].get("receipt", receipt)   # a fresh receipt; the old one keeps working

# retry later instead of waiting for the timeout
s.post(f"{Q}/nack", json={"receipts": [receipt], "requeue": True, "delay_ms": 5000})

# give up now: dead-letter (reason "rejected")
s.post(f"{Q}/nack", json={"receipts": [receipt], "requeue": False})
```

Settling a receipt after its visibility timeout gives `410 receipt_expired`. The message has
gone back to the queue, so receive it again. `409 receipt_stale` means it was already settled
or redelivered.

### After (fetch, Node or edge)

```js
const ROLL = 'http://127.0.0.1:55680' // hosted: https://<your-endpoint>
const Q = `${ROLL}/v1/vhosts/%2F/queues/orders`
const headers = { 'Content-Type': 'application/json', Authorization: `Bearer ${process.env.ROLL_TOKEN}` }
const post = (path, body) => fetch(`${Q}/${path}`, { method: 'POST', headers, body: JSON.stringify(body) })

await post('messages', { body: { order: 1042 }, headers: { tenant: 'acme' } })

for (;;) {
  const { messages } = await (await post('receive', { max: 10, wait_ms: 20000, visibility_ms: 60000 })).json()
  for (const m of messages) await handle(JSON.parse(m.body))
  if (messages.length) await post('ack', { receipts: messages.map((m) => m.receipt) })
}
```

### Batch send

```python
# sqs.send_message_batch(QueueUrl=URL, Entries=[{"Id": "1", "MessageBody": "a"}, ...])  # up to 10
s.post(f"{Q}/messages", json={"messages": [{"body": "a"}, {"body": "b"}]})              # up to 500
# → {"results": [{"id": "…", "routed": true, "confirmed": true}, …]}
```

### Push instead of polling

- **Server-Sent Events.** `GET …/queues/orders/events?ack=manual&prefetch=20` streams messages
  with receipts. You settle them with the same `ack` / `nack` endpoints.
- **WebSocket.** `GET …/queues/orders/ws` consumes and publishes over one socket.

See [http-api.md](http-api.md#server-sent-events).

## Delayed messages

`DelaySeconds` maps to an `x-delayed-message` exchange. Declare it once and bind the queue:

```sh
curl -u guest:guest -X PUT $ROLL/v1/vhosts/%2F/exchanges/orders.delayed \
  -d '{"type": "x-delayed-message", "arguments": {"x-delayed-type": "direct"}}'
curl -u guest:guest $ROLL/v1/vhosts/%2F/bindings \
  -d '{"source": "orders.delayed", "destination": "orders", "routing_key": "orders"}'
# or: roll queues exchanges declare orders.delayed --type x-delayed-message --arg x-delayed-type=direct
#     roll queues bind orders.delayed orders --key orders
```

Then publish through it with `x-delay` in milliseconds:

```sh
curl -u guest:guest $ROLL/v1/vhosts/%2F/exchanges/orders.delayed/publish \
  -d '{"routing_key": "orders", "body": {"order": 1042}, "headers": {"x-delay": 30000}}'
```

Delayed messages are stored durably when persistent, which is the HTTP default. There is no
per-queue default delay; set `x-delay` on each message.

## FIFO queues

roll doesn't have message groups. What it offers:

- **One ordered stream.** A queue with `x-single-active-consumer: true` delivers to one
  consumer at a time, in order. Other consumers wait as standbys. This applies to subscribed
  consumers: AMQP, SSE, WebSocket and webhooks. HTTP `receive` leases are not subject to
  single active consumer. For strict order over HTTP, run one receiver.
- **Partitions.** `x-partitions: N` spreads a queue over N partitions by the `x-partition-key`
  header, or by message id. Order holds within a partition. Consumers read from every
  partition, so this adds throughput, not per-group ordering for parallel consumers.
- **Redelivery keeps position.** A message whose visibility timeout expires goes back to its
  original place in the queue, not to the end.

If you rely on many groups processed in parallel, each in order, split the groups across
queues (one per shard), each with a single active consumer.

## Deduplication

- **`id`.** A message published with the same `id` within 5 minutes, to the same exchange
  with the same routing key, isn't published again. The response has `"duplicate": true`.
  This matches the SQS FIFO deduplication interval. Use your `MessageDeduplicationId` as `id`.
- **`Idempotency-Key` header.** It makes the whole request safe to retry for 24 hours,
  scoped to the user or token and the endpoint.
- Content-based deduplication isn't built in. Send a hash of the body as `id`.

## Lambda triggers → webhooks

A webhook consumes a queue and POSTs each message to a URL, such as a Lambda function URL or
API Gateway:

```sh
curl -u guest:guest -X PUT $ROLL/v1/vhosts/%2F/webhooks/orders-to-lambda -d '{
  "queue": "orders",
  "url": "https://abc123.lambda-url.us-east-1.on.aws/",
  "max_in_flight": 20,
  "retry": {"max_attempts": 5, "backoff_ms": 1000, "max_backoff_ms": 60000}
}'
```

- A `2xx` acknowledges the message. Anything else, or a timeout, is retried with backoff.
  After the last attempt the message is rejected, so it is dead-lettered if the queue has a
  dead-letter exchange.
- There's one message per request, not an SQS `Records` batch, so change the handler to read a
  single [message](http-api.md#message-format).
- `max_in_flight` caps concurrency, as Lambda's maximum concurrency setting does for SQS.
- Verify the `Roll-Signature` header and deduplicate on `Roll-Message-Id`. Delivery is at
  least once. See [The request](http-api.md#the-request).

## IAM → users and tokens

| SQS / IAM                                        | roll                                                              |
| ------------------------------------------------ | ----------------------------------------------------------------- |
| a service role allowed `sqs:SendMessage` on a queue | a user with `write` on the exchange, or a token with `publish:<exchange-regex>` |
| `sqs:ReceiveMessage` / `sqs:DeleteMessage`        | `read` on the queue, or `consume:<queue-regex>`                   |
| `sqs:CreateQueue`                                 | `configure` on the queue, or `manage:<queue-regex>`               |
| temporary credentials                             | tokens with `expires_in`. Revoke with `DELETE /v1/tokens/{id}`.   |

```sh
roll queues users add billing --password '...'
roll queues permissions set billing --configure '^$' --write '^amq\.default$' --read '^orders$'

curl -u guest:guest $ROLL/v1/tokens -d '{
  "name": "storefront", "vhost": "/",
  "scopes": ["publish:^orders$"], "expires_in": 2592000
}'
```

Patterns are unanchored regexes, so anchor them. Publishing to a queue by name goes through the
default exchange, checked as `amq.default`. A token's `publish` scope may match either
`amq.default` or the queue name.

## Migration strategy

There is no SQS importer. Run both systems side by side and move traffic in steps.

1. **Create the queues** on roll, with their dead-letter queues, as above.
2. **Start roll consumers** next to the SQS consumers. Make handlers idempotent on a business
   key or the message id, because the next step can deliver a message through both systems.
3. **Move producers.** Either:
   - **Dual-write.** Publish each message to both SQS and roll, carrying the same id (an SQS
     message attribute on one side, `id` on the other). Both consumers see it, and the
     idempotent handler processes it once. When the roll side is healthy, stop publishing
     to SQS.
   - **Switch.** Move producers to roll one at a time behind a flag.
4. **Drain SQS.** Keep the SQS consumers running until `ApproximateNumberOfMessages` and
   `ApproximateNumberOfMessagesNotVisible` are 0 on the queue and its DLQ. Or copy what's
   left into roll:

   ```python
   import boto3, requests

   sqs = boto3.client("sqs")
   SRC = "https://sqs.us-east-1.amazonaws.com/123456789012/orders"
   Q = "http://127.0.0.1:55680/v1/vhosts/%2F/queues/orders"
   s = requests.Session(); s.auth = ("guest", "guest")

   while True:
       r = sqs.receive_message(QueueUrl=SRC, MaxNumberOfMessages=10, WaitTimeSeconds=20,
                               MessageAttributeNames=["All"])
       msgs = r.get("Messages", [])
       if not msgs:
           break
       batch = [{"id": m["MessageId"], "body": m["Body"],
                 "headers": {k: v["StringValue"] for k, v in m.get("MessageAttributes", {}).items() if "StringValue" in v}}
                for m in msgs]
       out = s.post(f"{Q}/messages", json={"messages": batch}, timeout=60)
       out.raise_for_status()
       stored = [m for m, res in zip(msgs, out.json()["results"]) if res["confirmed"]]
       if stored:
           sqs.delete_message_batch(QueueUrl=SRC, Entries=[
               {"Id": str(i), "ReceiptHandle": m["ReceiptHandle"]} for i, m in enumerate(stored)])
   ```

   The SQS `MessageId` becomes the roll `id`, so a retry within 5 minutes doesn't publish
   twice. Messages are deleted from SQS only after roll has stored them.
5. **Remove SQS.** Delete the Lambda event source mappings, then the queues and their IAM
   policies.

## Differences / not supported

- **No SQS API.** No SigV4, no SQS wire protocol, no queue URLs. Code changes from SDK calls to
  HTTP, or to any RabbitMQ client over AMQP.
- **No IAM.** Authentication is HTTP Basic with broker users, or bearer tokens.
- **FIFO message groups** have no direct equivalent. See [FIFO](#fifo-queues).
- **Content-based deduplication** isn't built in.
- **Queue-level `DelaySeconds`** doesn't exist. Delay per message through a delayed exchange.
- **Retention.** Messages don't expire unless you set `x-message-ttl`. Expired messages are
  dead-lettered when a dead-letter exchange is set.
- **Lambda batching.** Webhooks send one message per request.
- **Queue management over HTTP** covers list, declare, depth, purge and delete, plus
  exchanges and bindings. Queue attributes can't be changed after a declare (as in AMQP):
  use a [policy](migrate-from-rabbitmq.md#what-the-import-does) through the CLI, the roll API
  or the management API.
- **Limits differ.** Up to 100 messages per receive, 500 per publish batch and 1000 receipts per
  settle. Bodies up to 16 MiB by default.
- **Stream queues** can't be leased with `receive`. Read them with SSE or the WebSocket.
