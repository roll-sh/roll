# Migrate from CloudAMQP

A CloudAMQP instance runs RabbitMQ, so the flow is the one in
[Migrate from RabbitMQ](migrate-from-rabbitmq.md). This page covers where to find the values
in the CloudAMQP console and what to check for your plan.

```sh
export ROLL_API_URL=http://127.0.0.1:7070     # roll API (`roll` CLI)
export ROLL_MGMT=http://127.0.0.1:55673       # roll's RabbitMQ management API
```

## 1. Collect the instance details

In the CloudAMQP console, open the instance. The details page shows:

- the **AMQP URL**, `amqp(s)://<user>:<password>@<host>/<vhost>`. Take the user, password,
  host and vhost from it.
- the **RabbitMQ manager**, the management UI and API on `https://<host>`. This is the
  management URL you give roll.

```sh
export SRC=https://<host>              # management URL
export ROLL_MIGRATE_USER=<user> ROLL_MIGRATE_PASSWORD=<password>
```

Check what you can read before you migrate:

```sh
curl -su $ROLL_MIGRATE_USER:$ROLL_MIGRATE_PASSWORD $SRC/api/overview | jq '.listeners[] | {protocol, port}'
curl -su $ROLL_MIGRATE_USER:$ROLL_MIGRATE_PASSWORD -o /dev/null -w '%{http_code}\n' $SRC/api/definitions
```

- **Listeners.** Live migration connects its shovels to the management URL's host, on the
  port of the `amqp/ssl` listener for an https management URL (otherwise `amqp`). If those
  aren't the host and port in your AMQP URL, pass your AMQP URL with `--amqp-uri` (API:
  `source.amqp_uri`); its credentials default to `--user` / `--password`.
- **Definitions.** `/api/definitions` needs the `administrator` tag. If it returns 401 or
  403, your plan's user may not have that tag. Use the per-vhost export, which roll's
  `--vhost` option reads (`/api/definitions/<vhost>`). If that is refused too, let your
  applications declare their topology on roll as they connect, and move messages with
  hand-made shovels.

## 2. Plan notes

- **Shared plans** give you a vhost on a multi-tenant RabbitMQ. Migrate that one vhost:

  ```sh
  roll queues migrate --from $SRC --vhost <vhost> --move-messages
  ```

  A per-vhost export has no users or permissions. Create the application user on roll:

  ```sh
  roll queues vhosts create <vhost>                 # the import creates it too
  echo "$PASSWORD" | roll queues users add <user>
  roll queues permissions set <user> --vhost <vhost>
  ```

- **Dedicated plans** give you the whole broker. If your user can read `/api/definitions`,
  migrate every vhost at once:

  ```sh
  roll queues migrate --from $SRC --move-messages
  ```

  The import keeps users' password hashes, so applications sign in with their current
  passwords.

Either way, follow the progress with `roll queues migrate --watch`. Before you cut over, check
`state` and `reason` with `roll --json queues migrate`.

The same request through the API:

```sh
curl -X POST $ROLL_API_URL/v1/queues/migrate -H 'content-type: application/json' -d '{
  "source": {"management_url": "https://<host>", "username": "<user>", "password": "<password>", "vhost": "<vhost>"},
  "move_messages": true
}'
```

Leave out `vhost` to migrate everything. For a move spread over days, use
[federation](migrate-from-rabbitmq.md#c-federation-for-a-gradual-move) with your AMQP URL as
the upstream `uri`.

## 3. Change client URLs

Replace the CloudAMQP AMQP URL in your environment. The vhost stays the same.

| target        | URL                                                     |
| ------------- | ------------------------------------------------------- |
| CloudAMQP     | `amqps://<user>:<password>@<host>/<vhost>`              |
| roll (hosted) | `amqps://<user>:<password>@<your-endpoint>:5671/<vhost>` |
| roll (local)  | `amqp://<user>:<password>@127.0.0.1:55672/<vhost>`       |

Many CloudAMQP apps read `CLOUDAMQP_URL` or `AMQP_URL`. Change that one variable:

```sh
roll queues url --vhost <vhost>
# amqp://guest:guest@127.0.0.1:55672/<vhost>
```

`roll queues url` prints the URL for roll's default user. It includes the password only while
that password is still `guest`. Put your application user and password into it. Per-library snippets are in
[the client table](migrate-from-rabbitmq.md#client-connection-strings).

Other endpoints:

- MQTT moves to port 51883, or 55675 `/ws` for WebSocket.
- STOMP moves to port 56613, or 55674 `/ws` for WebSocket.
- Management tooling and Prometheus move to roll's management API, port 55673. Metrics are on
  `/metrics`.

## 4. Cut over and close the instance

1. Move consumers to roll, then producers
   ([cutover order](migrate-from-rabbitmq.md#cutover-order)).
2. Wait for `remaining: 0` on every `roll-migrate-*` shovel and no connections on the source:

   ```sh
   curl -su $ROLL_MIGRATE_USER:$ROLL_MIGRATE_PASSWORD $SRC/api/connections | jq length
   ```

3. Stop the migration. This deletes the shovels on roll, which hold the CloudAMQP credentials:

   ```sh
   roll queues migrate --stop
   ```

4. Delete the instance in the CloudAMQP console.

Run the [verification checklist](migrate-from-rabbitmq.md#verification-checklist) before
step 4.

## Cost

**Queues → Usage** in the dashboard (`/dashboard/queues/usage`) and `roll queues usage` price
your metered workload on roll. They also show the CloudAMQP plan that fits it: a shared plan
for small workloads, otherwise a dedicated 3-node cluster. Read the figures there, because
they follow the code's current price tables.

## Differences / not supported

Everything in [the RabbitMQ guide's list](migrate-from-rabbitmq.md#differences--not-supported)
applies. Specific to CloudAMQP:

- **Plugins.** Only the exchange types and plugins listed in the
  [feature parity table](migrate-from-rabbitmq.md#feature-parity) exist on roll. If your
  instance has other plugins enabled, check that you don't depend on them.
- **CloudAMQP's console features** (alarms, log streaming, integrations) are not part of roll.
  Use roll's `/metrics` with your own Prometheus and alerting.
- **Remapped ports.** If `/api/overview` reports ports that differ from your AMQP URL, pass
  the URL with `--amqp-uri`.
