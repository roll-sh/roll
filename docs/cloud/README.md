# roll cloud: every product on one fleet

How roll runs as a hosted service: one fleet of identical `rolld` nodes on AWS in us-east-1,
every node running every product, Cloudflare in front, and no downtime for scaling, updates or a
lost node or zone. This is the design every part of the deployment is built against; the status
of each part is at the end.

```
  customers: browsers, git, psql, redis-cli, Kafka/CQL/AMQP/MQTT clients, S3 SDKs, agents
          │ HTTP (proxied)                              │ TCP (DNS only)
     Cloudflare: DNS for roll.cloud and roll.site, WAF, cache, the CDN Worker
          │                                             │
     AWS Network Load Balancer, 3 zones, TCP passthrough, PROXY protocol v2
          │
   rolld   rolld   rolld   …   ECS on Graviton EC2, one node per instance, autoscaled
     TLS ends here (SNI picks the product and the tenant); any node accepts any connection
     and hands it to the owner of what it names
          │                    │                         │
   DynamoDB: who's alive,   S3: all data (the source   the system database: a roll postgres
   who owns what            of truth for every product) database on the fleet (catalogs, accounts)
```

## Principles

- **One fleet, one binary.** Every node runs the same `rolld` with every product on. There is
  no per-product server group to deploy, scale or patch. Per-product switches turn a product
  off without a redeploy.
- **Everything has one owner, and ownership moves.** Each tenant resource (a repo, a Postgres
  endpoint, a Redis database, a NoSQL database, a Kafka cluster, a storage account, a queue, a
  tasks tenant) has one owner node at a time, held through a lease. Any node accepts a request or
  connection, finds the owner and forwards it. A stopping node hands everything it owns to the
  others before it exits; a dead node's leases lapse and the others take over. This is roll
  code's cluster design (docs/code/cluster.md), lifted out for every product.
- **S3 is the source of truth.** Node disks are caches. A node can be replaced at any time.
- **Correctness never depends on leases.** Leases are about where work runs. Every write that
  must not happen twice is fenced (a compare-and-swap in the database, or a create-only object
  in S3), so two nodes that briefly both believe they own something can't corrupt it.
- **Nothing the fleet needs to start lives on the fleet.** Membership and leases are in
  DynamoDB, a serverless AWS service. The system database is a roll postgres database whose owner
  is chosen through DynamoDB like everything else, so the fleet can boot, and recover from a full
  outage, with nothing but DynamoDB and S3.

## Coordination: DynamoDB

One on-demand table, `roll-cluster` (partition key `pk`, sort key `sk`), replicated across three
zones by AWS:

| item | pk / sk | what |
| --- | --- | --- |
| node | `node` / `<node id>` | advertise URL, version, zone, draining flag, heartbeat counter |
| lease | `lease` / `<key>` | holder node, epoch, record version |
| singleton | `lease` / `singleton:<name>` | one-per-fleet jobs (ACME, GC per bucket, the land forwarder) |

- **No clocks are compared.** DynamoDB has no server clock, so liveness works like the AWS
  DynamoDB lock client: the holder rewrites a record version number (a random token) every
  heartbeat; a contender that watches a record and sees the same version for a full lease
  duration, on its own monotonic clock, may take it with a conditional write on that version and
  a bumped epoch. Holders stop acting at lease minus a safety margin since their last successful
  renewal, on their own monotonic clock (self-fencing), exactly as in code-cluster.
- **Renewals are per node, not per lease.** A node heartbeats its `node` item; a lease is valid
  while its holder's node item is live and the epoch is unchanged, so the table's write rate is
  nodes × heartbeat rate, not leases × rate. Placement is rendezvous hashing over live nodes.
- Cost at test scale: a few million writes a month, about $1–3.
- Single-node and local development keep using the in-process (or Postgres) backend:
  `roll-cluster` has a backend trait with `Memory`, `Postgres` (today's code-cluster) and
  `DynamoDb` implementations.

## Shared state: the system database

Catalogs, accounts, tokens and every product's control-plane metadata move off per-node SQLite
files into one database through `code-sql` (SQLite on a single node, Postgres in a cluster; SQL
written once in SQLite's dialect), as roll code's did.

- In the cloud it's a roll postgres database on the fleet: lease `system:pg`, always on, a hot
  standby on a second node, point-in-time recovery from S3.
- `ROLL_SYSTEM_DATABASE` takes any Postgres URL, so the fleet also runs on an outside Postgres
  (CI and local clusters use `postgres:18-alpine`).
- Boot order: nodes join through DynamoDB → the `system:pg` owner starts the system database from
  S3 → products connect → tenant resources get owners on first use or from their leases.

## Ownership keys

| product | key | notes |
| --- | --- | --- |
| code | `code:repo:<id>`, `agents:<owner id>` | already built (code-cluster) |
| postgres | `pg:endpoint:<id>`, `system:pg` | the engine runs in-process; compute moves with the lease |
| redis | `redis:db:<id>` | the owner serves RESP and REST; others splice connections to it |
| nosql | `nosql:db:<id>` | replaces the manifest's owner field; fences opens |
| streams | `stream:cluster:<id>` | a tenant's cluster on one owner at first; per partition later |
| storage | `storage:account:<id>` | metadata log per account instead of one per deployment |
| queues | `mq:<vhost>/<queue>` | mq-cluster's owner records move onto the shared layer |
| tasks | `tasks:tenant:<id>` | replaces the tasks cell's own lease |

## The edge

- **Cloudflare** hosts DNS for both zones. HTTP for the dashboard, the API and roll.site goes
  through Cloudflare's proxy (TLS mode Full (strict), WAF, cache). Everything that is a raw TCP
  protocol, or whose bodies can pass Cloudflare's 100 MB request limit (git pushes, S3 uploads),
  is DNS only and goes straight to the NLB.
- **The NLB** passes TCP through on every port, across zones, with PROXY protocol v2 so nodes see
  client addresses. Nothing terminates TLS before rolld.
- **rolld terminates TLS** (rustls) on every public listener and picks the certificate, the
  product and the tenant from SNI. Postgres accepts both the SSLRequest upgrade and direct TLS
  (`sslnegotiation=direct`), and clients without SNI can name the endpoint in
  `options=endpoint=<id>`, as on Neon.
- **Certificates** are Let's Encrypt wildcards issued over ACME DNS-01 through the Cloudflare
  API, by one node at a time (singleton `acme`), stored sealed in S3 (`system/certs/`) and
  reloaded by every node without a restart. Renewal starts 30 days before expiry.
- **Forwarding** to the owner is a signed internal hop on the private network: HTTP requests are
  forwarded as in code-cluster; TCP connections are spliced after the first message that names
  the tenant (startup packet, AUTH/HELLO, SASL, CQL STARTUP/AUTH, AMQP vhost).

### Hostnames

| hostname | what | Cloudflare |
| --- | --- | --- |
| `roll.cloud` | dashboard | proxied |
| `api.roll.cloud` | every product's HTTP API, MCP, agents' WebSocket | proxied |
| `git.roll.cloud` | git smart HTTP (`git.roll.cloud/<owner>/<repo>.git`) | DNS only |
| `<id>.pg.us-east-1.roll.cloud:5432` | Postgres endpoints | DNS only |
| `<id>.redis.us-east-1.roll.cloud:6379` (TLS), `https://<id>.redis.us-east-1.roll.cloud` (REST) | Redis | DNS only |
| `<id>.kafka.us-east-1.roll.cloud:9092` (TLS + SASL), HTTPS REST on 443 | streams | DNS only |
| `<id>.cql.us-east-1.roll.cloud:9042` (TLS), HTTPS Alternator on 443 | nosql | DNS only |
| `<id>.mq.us-east-1.roll.cloud` 5671 AMQPS, 8883 MQTTS, 61614 STOMP TLS, 5551 streams, 443 web | queues | DNS only |
| `s3.us-east-1.roll.cloud`, `<bucket>.s3.us-east-1.roll.cloud` | S3 API (path and virtual-host style) | DNS only |
| `tasks.us-east-1.roll.cloud` | tasks API | DNS only |
| `<bucket>.roll.site`, custom domains | public buckets and sites (the CDN Worker) | proxied |
| `chg-<id>.roll.site` | code previews | proxied |
| `origin.us-east-1.roll.cloud` | the NLB, the target of every record above | DNS only |

- **roll.cloud** is the product; **roll.site** only ever serves user content, so nothing a user
  uploads shares cookies, origin or reputation with the dashboard (roll.site goes on the Public
  Suffix List). The region in hostnames leaves room for more regions without renaming anything.
- Wildcards are one record each (`*.pg.us-east-1` and so on) pointing at the NLB.

## Deploys, scaling and failure

- **Pipeline** (`.github/workflows/deploy.yml`): every merge to `main` builds one arm64 image
  (rolld plus the dashboard), pushes it to ECR through GitHub OIDC (no stored AWS keys), then:
  1. **canary**: one node of the new version joins the fleet and takes its share; it bakes for
     10 minutes while alarms watch errors, latency, restarts and handoff failures;
  2. **roll**: the rest are replaced one at a time, each new node healthy before the next old one
     stops;
  3. **rollback**: any alarm, failed health check or ECS circuit breaker puts the previous image
     back the same way.
- **Version skew.** Old and new nodes run side by side during a deploy. Schema migrations are
  expand-then-contract (add now, remove a release later), internal routes accept the previous
  version's shapes, and a node refuses to start against a schema newer than it understands.
- **Stopping a node** (deploy, scale-in, Spot interruption): it fails its health check, waits for
  the NLB to deregister it, hands every lease it holds to the others (each product drains:
  requests in flight finish, connections are closed gradually so clients reconnect through
  another node), then exits. Stop timeouts exceed drain time plus margin.
- **A dead node or zone**: its leases lapse within about 10–15 s and the others take over.
  Acknowledged writes are never lost. ECS replaces the capacity in healthy zones.
- **Scaling**: the ECS capacity provider and the instance group grow on CPU, memory and
  connection count, with a floor of one node per zone. Spot instances take part; their two-minute
  notice starts a drain.

## Isolated work

Code checks, previews and agent sandboxes run customer code. They run as their own Fargate tasks
(ECS `RunTask`, a microVM each) with a minimal task role and no route into the fleet's network,
never on a fleet node or through a Docker socket.

## GitHub resilience

- **GitHub down**: roll lands, queues and reviews without it. The mirror catches up with one push.
  Everything roll tells GitHub (PR comments and closes, issue closes, check runs, reviews) goes
  through the durable delivery queue (`review_deliveries`, target `gh_…`) and retries until
  GitHub answers.
- **roll down**: GitHub doesn't redeliver failed webhooks on its own. roll asks it to (the App's
  delivery log, kept three days) and reconciles connected repos' branches and pull requests for
  anything older. The `roll` CLI and the agent daemon fall back to pushing a branch and opening a
  pull request on GitHub with the user's own credentials; roll lands it through the queue when
  it's back.

## Security

- Every control-plane API requires a token; nothing is open by default and CORS is limited to the
  dashboard's origin. Data planes keep their own credentials (Postgres passwords, Redis users,
  S3 keys, SASL).
- Secrets live in Secrets Manager and are sealed with KMS; nodes get them through their task
  role. Internal hops are signed with the cluster secret and stay on the VPC.
- Security groups admit the NLB and the other nodes only; Cloudflare's proxied hostnames reach
  the origin with authenticated origin pulls.

## Cost (us-east-1, testing size)

| piece | per month |
| --- | --- |
| 3 × t4g.medium fleet nodes, one per zone (Spot about a third of this) | ~$75 |
| NLB | ~$18 |
| public IPv4 addresses | ~$22 |
| DynamoDB, S3, logs, alarms, secrets, ECR | ~$12–17 |
| **total** | **~$125** (~$80 on Spot) |

Production is a parameter change: bigger instances with local NVMe, more nodes, and a separate
NLB per protocol family if connection counts need it.

## Node settings

The contract between the stack, the image and rolld. Each setting is a flag with this env name.

| env | value in the cloud | |
| --- | --- | --- |
| `ROLL_CLUSTER_BACKEND` | `dynamodb` | `memory` (one node, the default), `postgres`, `dynamodb` |
| `ROLL_CLUSTER_TABLE` | `roll-cluster` | the DynamoDB table |
| `ROLL_CLUSTER_SECRET` | from Secrets Manager | signs internal hops; `ROLL_CODE_CLUSTER_SECRET` stays an alias |
| `ROLL_NODE_ID` | the EC2 instance id | |
| `ROLL_ZONE` | the instance's availability zone | placement spreads standbys across zones |
| `ROLL_ADVERTISE_URL` | `http://<private ip>:7070` | where other nodes reach this one |
| `ROLL_API` | `0.0.0.0:7070` | internal HTTP: node-to-node hops and `GET /healthz`; never public |
| `ROLL_SYSTEM_DATABASE` | the system database's URL | catalogs and accounts for every product |
| `ROLL_S3_BUCKET`, `AWS_REGION` | `roll-cloud-data-<account>`, `us-east-1` | all product data, under per-product prefixes |
| `ROLL_SECRET_KEY` | from Secrets Manager | seals owners' keys and the stored certificates |
| `ROLL_ADMIN_TOKEN` | from Secrets Manager | the operator token every control-plane API accepts |
| `ROLL_DASHBOARD_ORIGIN` | `https://roll.cloud` | the only origin CORS allows |
| `ROLL_EDGE` | `1` | public listeners with TLS, SNI routing and PROXY protocol |
| `ROLL_EDGE_DOMAIN`, `ROLL_EDGE_SITE_DOMAIN`, `ROLL_EDGE_REGION` | `roll.cloud`, `roll.site`, `us-east-1` | hostnames are built from these |
| `ROLL_EDGE_PROXY_PROTOCOL` | `1` | expect PROXY protocol v2 from the NLB |
| `ROLL_ACME_DIRECTORY`, `ROLL_ACME_EMAIL` | Let's Encrypt production, ops address | |
| `ROLL_CLOUDFLARE_API_TOKEN` | from Secrets Manager | DNS edit on both zones, for DNS-01 only |
| `ROLL_DASHBOARD_UPSTREAM` | `http://127.0.0.1:3000` | the dashboard container in the same task |

Public ports (NLB listener → node port): 443 → 8443 (HTTPS: dashboard, API, git, S3, REST data
planes, WebSockets), 5432, 6379, 9092, 9042, 5671, 8883, 61614 and 5551, each to the same port on
the node. The NLB's health check is `GET http://<node>:7070/healthz`: 200 while the node serves and
isn't draining.

## Where everything lives

| part | path |
| --- | --- |
| AWS stack | `deploy/cloud/aws/roll-cloud.yaml` |
| Cloudflare zones and records | `deploy/cloud/cloudflare/` |
| fleet image | `deploy/cloud/Dockerfile` |
| deploy pipeline | `.github/workflows/deploy.yml` |
| cluster layer | `crates/roll-cluster` (from `crates/code-cluster`) |
| edge (TLS, SNI, ACME, PROXY protocol, routing) | `crates/roll-edge` |

## Status

| part | state |
| --- | --- |
| code + agents on the cluster layer | built (Postgres leases); DynamoDB backend in progress |
| AWS stack, image, pipeline, Cloudflare | in progress |
| edge | in progress |
| control-plane auth lockdown | in progress |
| GitHub resilience | in progress |
| tasks, queues, storage, redis, streams, nosql on the cluster layer | next, one product per change |
| postgres on the fleet and the system database | with the pg-engine branch |
| isolated work on Fargate | next |
