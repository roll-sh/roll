# Migrate from Amazon MQ for RabbitMQ

An Amazon MQ for RabbitMQ broker is a RabbitMQ, so the flow is the one in
[Migrate from RabbitMQ](migrate-from-rabbitmq.md). This page covers what differs on Amazon MQ:
TLS-only endpoints, the management URL, network access and decommissioning.

```sh
export ROLL_API_URL=http://127.0.0.1:7070     # roll API (`roll` CLI)
export ROLL_MGMT=http://127.0.0.1:55673       # roll's RabbitMQ management API
export SRC=https://b-1234abcd-....mq.us-east-1.amazonaws.com   # broker web console URL
```

## 1. Find the broker's endpoints

In the Amazon MQ console, open the broker. Under **Connections** you'll find:

- the **web console URL**, `https://b-….mq.<region>.amazonaws.com`. This is the RabbitMQ
  management API. Pass it to roll as the management URL, with or without a trailing `/api`.
- the **AMQP endpoint**, `amqps://b-….mq.<region>.amazonaws.com:5671`. Amazon MQ accepts
  AMQP over TLS only.

Check what the broker reports, because live migration builds its shovel URIs from it:

```sh
curl -su admin:... $SRC/api/overview | jq '.listeners[] | {protocol, port}'
```

Live migration uses the management URL's host and, because the web console URL is https, the
`amqp/ssl` listener's port (the `amqp` port if there's no TLS listener). If the result isn't
`amqps://<broker host>:5671`, pass the endpoint explicitly; the credentials default to the
migration user:

```sh
roll queues migrate --from $SRC --move-messages --amqp-uri amqps://b-1234abcd-....mq.us-east-1.amazonaws.com:5671
```

## 2. Network and access

roll connects to the broker. Nothing connects from the broker to roll.

| roll needs to reach              | port | for                                     |
| -------------------------------- | ---- | --------------------------------------- |
| the web console URL              | 443  | definitions (`/api/definitions`, `/api/overview`) |
| the AMQP endpoint                | 5671 | shovels or federation links (moving messages) |

- **Private brokers** are only reachable inside their VPC. Run roll in that VPC or a peered
  one, and allow 443 and 5671 from roll's hosts in the broker's security group.
- **Public brokers** are reachable over the internet; allow roll's egress to them.
- **Credentials.** Use a broker user with the `administrator` tag; `/api/definitions` needs
  it. The same user moves messages over AMQP, so it needs `read` on every queue you move.
  roll signs in to the broker as this RabbitMQ user, with HTTP Basic and AMQP `PLAIN`. It
  doesn't use AWS credentials. IAM matters only for the AWS side: network setup and deleting
  the broker at the end.
- **LDAP or OAuth 2.0 brokers.** Users that come from LDAP or OAuth 2.0 aren't in the
  definitions, and roll has no LDAP or OAuth backend. Create them on roll:

  ```sh
  echo "$PASSWORD" | roll queues users add app
  roll queues permissions set app --vhost /       # default: .* for configure, write, read
  ```

## 3. Migrate

Live migration, with the CLI:

```sh
export ROLL_MIGRATE_USER=admin ROLL_MIGRATE_PASSWORD=...
roll queues migrate --from $SRC --move-messages
roll queues migrate --watch
```

Or with the API:

```sh
curl -X POST $ROLL_API_URL/v1/queues/migrate -H 'content-type: application/json' -d '{
  "source": {"management_url": "https://b-1234abcd-....mq.us-east-1.amazonaws.com", "username": "admin", "password": "..."},
  "move_messages": true
}'
```

The import brings vhosts, users with their password hashes, permissions, policies,
exchanges, queues, bindings and parameters. Then the `roll-migrate-<queue>` shovels move
messages over `amqps`. After the import:

- Review `roll queues users`. Delete users you don't want on roll.
- Review what was skipped (printed by the CLI, `skipped` in the API response). Classic
  mirroring (`ha-*`) policy keys are dropped. The broker's own shovels and federation
  upstreams are left out unless you pass `--include-shovels` (`"include_shovels": true`).

For a slower move, use [federation](migrate-from-rabbitmq.md#c-federation-for-a-gradual-move)
with an `amqps://…:5671` upstream URI instead.

## 4. Change client URLs

Amazon MQ clients use `amqps` on 5671. Change the host, and the scheme and port if your roll
endpoint differs:

| target          | URL                                                       |
| --------------- | --------------------------------------------------------- |
| Amazon MQ       | `amqps://app:secret@b-….mq.us-east-1.amazonaws.com:5671/%2F` |
| roll (hosted)   | `amqps://app:secret@<your-endpoint>:5671/%2F`               |
| roll (local)    | `amqp://app:secret@127.0.0.1:55672/%2F`                     |

`rolld` itself serves plain TCP, so local URLs use `amqp://`. If your client sets TLS options
in code, as well as through the URL scheme, turn them off for local `rolld`. Per-library
snippets are in [the client table](migrate-from-rabbitmq.md#client-connection-strings).

Management tooling (`rabbitmqadmin`, the Terraform `rabbitmq` provider) and Prometheus move
from the web console URL to roll's management API (`http://127.0.0.1:55673` locally,
`<your-endpoint>` hosted). Metrics are on `/metrics` of the same port.

## 5. Cut over

Follow the [cutover order](migrate-from-rabbitmq.md#cutover-order): consumers to roll, then
producers, then wait for `remaining: 0` on every shovel and check `state` and `reason`
(`roll --json queues migrate`; `roll queues migrate --watch` exits once all have drained).
Then stop the migration, which deletes the shovels:

```sh
roll queues migrate --stop
```

## 6. Decommission the broker

1. Confirm the broker has no connections and its queues are empty:

   ```sh
   curl -su admin:... $SRC/api/connections | jq length          # 0
   curl -su admin:... $SRC/api/queues | jq '[.[].messages] | add'   # 0 or null
   ```

2. Keep a copy of the definitions: `curl -su admin:... $SRC/api/definitions -o amazon-mq-definitions.json`.
3. Stop the migration (`roll queues migrate --stop`) and delete any federation upstreams on
   roll. They hold the broker's credentials.
4. Delete the broker (`aws mq delete-broker --broker-id <id>`, or from the console), and remove
   the security group rules you added for roll.

## Cost

The dashboard's **Queues → Usage** page (`/dashboard/queues/usage`) and `roll queues usage`
price your metered workload on roll. They also show what the same workload would cost on an
Amazon MQ 3-node cluster, which is billed while idle. The figures come from the code's current
price tables, so read them there rather than from this page.

## Differences / not supported

Everything in [the RabbitMQ guide's list](migrate-from-rabbitmq.md#differences--not-supported)
applies. Specific to Amazon MQ:

- **No LDAP or OAuth 2.0.** Recreate those users as roll users.
- **Remapped ports.** If `/api/overview` doesn't report 5671 for `amqp/ssl` on the broker
  host, pass the AMQP endpoint with `--amqp-uri`.
- **The web console isn't replicated.** roll serves the management API but not the RabbitMQ
  web UI. Use the roll dashboard.
