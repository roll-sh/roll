# roll queues docs

roll queues is a RabbitMQ-compatible broker. Clients speak AMQP 0-9-1, AMQP 1.0, MQTT, STOMP,
the RabbitMQ Stream protocol or HTTP to it, and the RabbitMQ management API, definitions and Prometheus metrics work as
they do on RabbitMQ.

## Reference

| doc                                   |                                                                  |
| ------------------------------------- | ---------------------------------------------------------------- |
| [architecture.md](architecture.md)    | how the broker is built: stateless brokers, replicated log, metadata, partitions, cost model |
| [contracts.md](contracts.md)          | ports, default credentials, crate conventions and the roll API (`/v1/queues/...`) |
| [http-api.md](http-api.md)            | the HTTP messaging API: publish, receive with receipts, SSE, WebSocket, webhooks, tokens |
| [storage.md](storage.md)              | the replicated log: keepers, quorum appends, fencing, S3 tiering, S3 Express |
| [deploy-aws.md](deploy-aws.md)        | the CloudFormation stack: a shared Graviton fleet (on demand + Spot), keepers per AZ, AZ-local proxies, tenants that scale to zero |

## Migration guides

| from                                                          | path                                                                      |
| ------------------------------------------------------------- | ------------------------------------------------------------------------- |
| [RabbitMQ](migrate-from-rabbitmq.md)                          | live migration, definitions plus shovels, or federation. Clients change only the URL. |
| [Amazon MQ for RabbitMQ](migrate-from-amazon-mq.md)           | the RabbitMQ flow over `amqps` and the broker's management URL            |
| [CloudAMQP](migrate-from-cloudamqp.md)                        | the RabbitMQ flow from the CloudAMQP console and management URL           |
| [Amazon SQS](migrate-from-sqs.md)                             | concept mapping onto the HTTP API, code changes, and a dual-run cutover   |

## Local endpoints

`rolld` listens on these addresses by default. Each one has a flag and an environment
variable (see `crates/rolld/src/queues.rs`).

| listener                  | default            | flag / env                                        |
| ------------------------- | ------------------ | ------------------------------------------------- |
| AMQP 0-9-1 and AMQP 1.0   | `127.0.0.1:55672`  | `--queues-amqp` / `ROLL_QUEUES_AMQP`              |
| Management API, `/metrics`| `127.0.0.1:55673`  | `--queues-management` / `ROLL_QUEUES_MANAGEMENT`  |
| STOMP                     | `127.0.0.1:56613`  | `--queues-stomp` / `ROLL_QUEUES_STOMP`            |
| STOMP over WebSocket `/ws`| `127.0.0.1:55674`  | `--queues-web-stomp` / `ROLL_QUEUES_WEB_STOMP`    |
| MQTT                      | `127.0.0.1:51883`  | `--queues-mqtt` / `ROLL_QUEUES_MQTT`              |
| MQTT over WebSocket `/ws` | `127.0.0.1:55675`  | `--queues-web-mqtt` / `ROLL_QUEUES_WEB_MQTT`      |
| RabbitMQ Stream protocol  | `127.0.0.1:55552`  | `--queues-stream` / `ROLL_QUEUES_STREAM`          |
| HTTP messaging API        | `127.0.0.1:55680`  | `--queues-http` / `ROLL_QUEUES_HTTP`              |
| roll API (`/v1/queues`)   | `127.0.0.1:7070`   | `--api` / `ROLL_API`                              |
| cluster RPC               | `0.0.0.0:55679`    | `--queues-rpc` / `ROLL_QUEUES_RPC` (cluster mode) |

- The first start creates `guest` / `guest` (an administrator) on vhost `/`. Change it with
  `--queues-user` / `--queues-password` (`ROLL_QUEUES_USER` / `ROLL_QUEUES_PASSWORD`).
  roll doesn't restrict `guest` to localhost, so set a real password before you expose a port.
- The roll API on `:7070` has no authentication. Keep it private.
- `roll queues url` prints the AMQP URL for a vhost.

## A vhost per app

Each app or environment gets its own vhost, its own user and one URL per protocol, so it works
like a database per branch on Neon. The vhost costs nothing while it sleeps and wakes on the
first connection.

```sh
roll queues vhosts create shop --credentials
# created vhost shop
# user      shop
# password  3f9c…  (shown once)
# amqp      amqp://shop:3f9c…@127.0.0.1:55672/shop
# mqtt      mqtt://shop%3Ashop:3f9c…@127.0.0.1:51883
# …
roll queues vhosts credentials shop     # rotate: new password, old one stops working
```

The user can reach only its vhost, and its `management` tag lets it log in to the management
API to see that vhost and nothing else. The dashboard does the same from **Vhosts → New vhost**.
The API calls are `POST /v1/queues/vhosts` with `"credentials": true` and
`POST /v1/queues/vhosts/{vhost}/credentials` (see [contracts.md](contracts.md)).
- For a cluster, start each node with `--queues-cluster-meta postgres://…`. Metadata and queue
  ownership then live in Postgres, and every node accepts every client.
