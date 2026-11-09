# roll cloud: every product on one fleet

How roll runs as a hosted service: one fleet of identical `rolld` nodes on AWS in us-east-1,
every node running every product, Cloudflare in front, and no downtime for scaling, updates or a
lost node or zone. This is the design every part of the deployment is built against; the status
of each part is at the end.

```
  customers: browsers, git, psql, redis-cli, Kafka/CQL/AMQP/MQTT clients, S3 SDKs, agents
          │ HTTP (proxied)                              │ TCP (DNS only)
     Cloudflare: DNS for roll.sh and roll.site, WAF, cache, the CDN Worker
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
  no per-product server group to deploy, scale or patch. A product can be switched off for the
  whole fleet with a setting (`ROLL_PRODUCTS`; "Products").
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
| node | `node` / `<node id>` | advertise URL, version, zone, draining flag, incarnation, heartbeat token (`hb`), takeover generation (`gen`), release counter |
| lease | `lease` / `<key>` | holder node and its incarnation, epoch, free flag, record version |
| singleton | `lease` / `singleton:<name>` | one-per-fleet jobs (ACME, GC per bucket, the land forwarder) |

- **No clocks are compared.** DynamoDB has no server clock, so liveness works like the AWS
  DynamoDB lock client: the holder rewrites a record version number (a random token) every
  heartbeat; a contender that watches a record and sees the same version for a full lease
  duration, on its own monotonic clock, may take it with a conditional write on that version and
  a bumped epoch. Holders stop acting at lease minus a safety margin since their last successful
  renewal, on their own monotonic clock (self-fencing), as on Postgres. The takeover is a
  transaction that also checks the holder's node item still carries the watched token and bumps
  its `gen`, so a renewal that lands after it fails and the holder re-reads its leases.
- **Renewals are per node, not per lease.** A node heartbeats its `node` item; a lease is valid
  while its holder's node item is live and the epoch is unchanged, so the table's write rate is
  nodes × heartbeat rate, not leases × rate. Placement is rendezvous hashing over live nodes.
- Cost at test scale: a few million writes a month, about $1–3.
- Single-node and local development keep using the in-process (or Postgres) backend:
  `roll-cluster` has a backend trait with `Memory`, `Postgres` (roll code's original tables, on
  the database's clock) and `DynamoDb` implementations (docs/code/cluster.md, "Backends").

## Shared state: the system database

Catalogs, accounts, tokens and every product's control-plane metadata move off per-node SQLite
files into one database through `code-sql` (SQLite on a single node, Postgres in a cluster; SQL
written once in SQLite's dialect), as roll code's did.

- In the cloud it's a roll postgres database on the fleet: lease `system:pg`, always on, a hot
  standby on a second node, point-in-time recovery from S3.
- `ROLL_SYSTEM_DATABASE` takes any Postgres URL, so the fleet also runs on an outside Postgres
  (CI and local clusters use `postgres:18-alpine`).
- roll code keeps its data there (schemas `code_store` and `code_control`) unless
  `ROLL_CODE_DATABASE` names a Postgres of its own (docs/code/cluster.md, "Code's database").
- Boot order: nodes join through DynamoDB → the `system:pg` owner starts the system database from
  S3 → products connect → tenant resources get owners on first use or from their leases.

## Ownership keys

| product | key | notes |
| --- | --- | --- |
| code | `<repo id>`, `agents:<owner id>` | built (roll-cluster); repo leases keep their bare ids so a running Postgres cluster upgrades in place |
| postgres | `pg:endpoint:<id>`, `system:pg` | the engine runs in-process; compute moves with the lease |
| redis | `redis:db:<id>`, `redis:db:<primary>#<replica>` | built: the owner serves RESP and REST; others splice connections to it; a replica is placed as a standby of its primary ("Redis on the fleet") |
| nosql | `nosql:db:<id>` | the database's owner loads it; opens and manifest writes are fenced by compare-and-swap in S3 |
| streams | `stream:cluster:<id>` | a tenant's whole Kafka cluster on one owner (the stream SASL maps a key to); acknowledged once in the owner's write-ahead log in S3; per partition later (`crates/stream-broker/README.md`, "On the fleet") |
| storage | `storage:meta` | built: one metadata log for the deployment, a hot standby following it (docs/storage/architecture.md, "Many nodes"); `storage:account:<id>` (a log per account) is designed there, not built |
| queues | `mq:<vhost>/<queue>` (`#<n>` per partition after the first) | built: every node serves every client (stream clients too, as one virtual node) and forwards to each queue's owner over the internal RPC (no splice); logs in S3 Standard, fenced by put-if-absent (docs/queues/architecture.md, "Many nodes") |
| tasks | `tasks:cell:default` | built: the log store cell's leader (replaces the cell's own S3 lease; fencing stays in the log), hot standbys following the log (deploy/aws/README.md, "On the roll cloud fleet"); `tasks:tenant:<id>` needs more cells and a tenant catalog, designed there, not built |

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
  forwarded as in roll-cluster (`Cluster::dispatch`); TCP connections are spliced after the first message that names
  the tenant (startup packet, AUTH/HELLO, SASL, CQL STARTUP/AUTH). Queues' connections aren't
  spliced: a connection's queues (and a stream connection's streams) can each have a different
  owner, so the node serves the connection itself and forwards each queue's operations to its
  owner.

### Hostnames

| hostname | what | Cloudflare |
| --- | --- | --- |
| `roll.sh` | the site and dashboard (`/dashboard`), public repo pages (`/<owner>/<repo>`) | proxied |
| `roll.sh/<owner>/<repo>.git` | git smart HTTP; the edge sends `.git` paths straight to code, not through the dashboard | proxied |
| `git.roll.sh/<owner>/<repo>.git` | the same, without Cloudflare's 100 MB request limit (big first pushes) | DNS only |
| `api.roll.sh` | every product's HTTP API, MCP, agents' WebSocket | proxied |
| `<id>.pg.us-east-1.roll.sh:5432` | Postgres endpoints | DNS only |
| `<id>.redis.us-east-1.roll.sh:6379` (TLS), `https://<id>.redis.us-east-1.roll.sh` (REST) | Redis | DNS only |
| `<id>.kafka.us-east-1.roll.sh:9092` (TLS + SASL), HTTPS REST on 443 | streams | DNS only |
| `<id>.nosql.us-east-1.roll.sh:9042` (TLS), HTTPS Alternator on 443 | nosql | DNS only |
| `<vhost>.mq.us-east-1.roll.sh` 5671 AMQPS, 8883 MQTTS, 61614 STOMP TLS, 5551 streams (TLS), 443 web | queues: one hostname per vhost (below) | DNS only |
| `tasks.us-east-1.roll.sh` | tasks API (`/v1/tasks`, hosted tenants at `/t/<tenant>/v1/tasks`) | DNS only |
| `s3.us-east-1.roll.site`, `<bucket>.s3.us-east-1.roll.site` | S3 API (path and virtual-host style) | DNS only |
| `<bucket>.roll.site`, custom domains | public buckets and sites (the CDN Worker) | proxied |
| `chg-<id>.roll.site` | code previews | proxied |
| `origin.us-east-1.roll.sh` | the region's NLB, the target of every DNS-only record above | DNS only |

- **roll.sh** is the product; **roll.site** serves only user content: anything a user uploaded
  or built that a browser could render (objects, presigned S3 URLs, previews, sites). Nothing
  there shares cookies, origin or reputation with the dashboard, the way S3 lives on
  amazonaws.com rather than amazon.com. roll.site goes on the Public Suffix List.
- Raw file content from the API (`api.roll.sh`, e.g. a repo's raw files) is served as
  `text/plain` or an attachment with `X-Content-Type-Options: nosniff`, never as HTML.
- **Hostnames customers connect to directly carry the AWS region** (`us-east-1`). Connection
  strings live in customers' code and can never be renamed, the region tells them where to run
  their apps for low latency and no cross-region transfer, and it's the region S3 clients sign
  with. Each region is one wildcard record per product (`*.pg.us-east-1.roll.sh` and so on)
  pointing at that region's NLB: no per-tenant DNS records and no global router.
- Global or Cloudflare-proxied names carry no region (`roll.sh`, `api.roll.sh`, `git.roll.sh`,
  `<bucket>.roll.site`, `chg-<id>.roll.site`): the API routes to a resource's region, and
  Cloudflare can send a proxied request to the right region when there is more than one.
- roll.cloud isn't needed. If it's bought, it only redirects to roll.sh.
- **Queues' `<vhost>`** is the vhost's DNS label: its name when that is one (lowercase letters,
  digits, inner hyphens, at most 63), `default` for `/`, else the name lowercased with each run of
  other characters made one hyphen (`mq_control::host_label`). A vhost is queues' tenant (its own
  user, one connection string per protocol: docs/queues/README.md, "A vhost per app"). The edge
  routes every `*.mq.<region>.<domain>` the same way and neither SNI nor the label picks the
  tenant: every node serves every vhost, and a connection opens the vhost its connection string
  names (AMQP's and STOMP's path, MQTT's `vhost:user`, the stream protocol's `Open`). The name
  is what the stream protocol hands back as its one virtual node, and what connection details
  print.

### The edge in rolld

`crates/roll-edge` is the edge; `crates/rolld/src/edge.rs` runs it in front of the products
when `ROLL_EDGE=1` (without it rolld is unchanged).

| listener | what happens |
| --- | --- |
| 8443 (HTTPS) | HTTP/1.1, routed by `Host`: `roll.sh` → `ROLL_DASHBOARD_UPSTREAM` (streamed, WebSockets included) except `/<owner>/<repo>.git/…` → code's git routes; `git.roll.sh` → code's git routes; `api.roll.sh` → rolld's API router; `<id>.redis.…` → Redis REST; `<id>.kafka.…` → the streams HTTP API; `<id>.nosql.…` → Alternator; `<id>.mq.…` → web-STOMP or web-MQTT by `Sec-WebSocket-Protocol`, else the queues HTTP API; `tasks.…` → tasks (`/v1/tasks/…`, `/t/<tenant>/v1/tasks/…`, `/_roll/tenants`; run on the cell's leader); `s3.…`, `<bucket>.s3.…` → the S3 server; `chg-<id>.roll.site` → code previews. `<bucket>.roll.site` answers 404 at the origin: it is served by the CDN Worker. |
| 5432 | `SSLRequest` → `S` → TLS, or TLS straight away (`sslnegotiation=direct`, ALPN `postgresql`); `GSSENCRequest` → `N`; a startup packet in the clear is refused (`08P01`); cancel requests may come in the clear. The endpoint is the server name's `<id>`, else `options=endpoint=<id>`; naming two different ones is an error. |
| 6379 | TLS, then Redis: the server name's first label is the database, and the connection is spliced to its owner right after the handshake (without a server name, from its first AUTH or HELLO … AUTH on). |
| 9092 | TLS, then Kafka; metadata names the server name the client used (one virtual broker per hostname), never a node address. |
| 9042 | TLS, then CQL with the server name (the database is its first label). |
| 5671, 8883, 61614, 5551 | TLS, then AMQP (0-9-1 and 1.0), MQTT, STOMP and the RabbitMQ stream protocol, served by this node, each queue's operations forwarded to its owner. On 5551 the connection sees one virtual node at the server name and port the client used (the default vhost's name without SNI): `Metadata` names it as every stream's leader, never a node address. |

- **Client addresses** come from the PROXY protocol header (v1 or v2, at most 512 bytes after
  the fixed part, read under the handshake deadline). On the HTTPS listener handlers get it as
  `ConnectInfo` and in `X-Forwarded-For` (whatever the client sent is dropped). For a proxied
  hostname whose peer is a Cloudflare address, `CF-Connecting-IP` is the client; the ranges are in
  `roll-edge/src/trusted.rs` and `ROLL_EDGE_CLOUDFLARE_IPS` replaces them.
- **Authenticated origin pulls.** Proxied hostnames (`roll.sh`, `www.roll.sh`, `api.roll.sh`,
  `*.roll.site`) are handshaken only with Cloudflare's origin-pull client certificate (the CA is
  `roll-edge/src/cloudflare-origin-pull-ca.pem`, valid to 2029-11-01; `ROLL_EDGE_ORIGIN_PULL_CA`
  replaces it), and a request naming one of them on a connection that didn't show it gets 403,
  whatever the TLS server name was. DNS-only hostnames ask for no client certificate.
- **Certificates**: two sets, so roll.sh and roll.site never share one: `roll.sh`, `api.roll.sh`,
  `git.roll.sh`, `tasks.us-east-1.roll.sh` and `*.{pg,redis,kafka,nosql,mq}.us-east-1.roll.sh`;
  and `*.roll.site`, `s3.us-east-1.roll.site`, `*.s3.us-east-1.roll.site`. Bucket names with
  dots aren't covered by the wildcard and use path style, as on S3. They're stored sealed under
  `ROLL_SECRET_KEY` (`system/certs/{product,site}.sealed`, plus the ACME account), every node
  polls them (`ROLL_EDGE_CERT_POLL_S`, 600) and installs a new one without a restart, and the node
  holding the `acme` singleton orders what's missing or within 30 days of expiry (backing off an
  hour after a failed order). `roll_edge::certs::Singleton` is the seam for roll-cluster's
  `singleton(name)`; rolld passes `LocalSingleton` until the merge.
- **Healthy** (`/healthz` 200) once a certificate is loaded and until SIGTERM. Then, in this
  order (`roll_edge::drain`), so a client moves once: `/healthz` fails at once; the public
  listeners keep accepting and serving for `ROLL_EDGE_DRAIN_DELAY` (15 s) while the NLB takes the
  node out; they stop accepting (a late connection is refused and goes to another node) and
  keep-alive HTTP connections close after their request in flight; the products drain, each
  closing its own tenants' connections gradually as it hands them off (Redis over 2 s, CQL over
  a second, queues over the 5–10 s after their queues moved, since their connections don't need to
  move); only then does the edge close the TCP connections still open, the ones no product
  here owns (spliced on to another node, or never past their first message). Tested with real
  clients by `redis-control/tests/cluster.rs::a_drain_hands_a_busy_database_over_losing_nothing`
  and `nosql-control/tests/cluster.rs::a_drain_hands_databases_over_with_nothing_lost`.
- **Forwarding TCP** to a tenant's owner: `roll_edge::splice::splice(client, owner_url, signer,
  hop, first_bytes)` opens an HTTP upgrade to the owner's `/internal/edge/splice` (HMAC-SHA256
  over the hop and the time under the cluster secret, 60 s skew), writes the bytes already read,
  then copies both ways. The owner hands the stream to the handler its product registered:
  Postgres's (the proxy, with the endpoint as named), Redis's, CQL's and Kafka's (`kafka`:
  spliced after SASL, the tenant is the stream the connection authenticated as, the server name
  carries the `host:port` the client was told). The route is on every cluster node's internal
  API, with the edge or without (rolld's own Redis, CQL and Kafka ports splice too;
  `rolld/src/edge.rs`, `splice_routes`).
- **Dev mode**: `ROLL_EDGE_CERT_DIR` serves its `<name>.crt`/`<name>.key` pairs (reloaded when
  they change) and writes a self-signed certificate for every edge name when it has none; origin
  pulls are off unless `ROLL_EDGE_ORIGIN_PULL=1`. Every listener's address is a setting
  (`ROLL_EDGE_HTTPS`, `ROLL_EDGE_PG`, `ROLL_EDGE_REDIS`, `ROLL_EDGE_KAFKA`, `ROLL_EDGE_NOSQL`,
  `ROLL_EDGE_AMQPS`, `ROLL_EDGE_MQTTS`, `ROLL_EDGE_STOMPS`, `ROLL_EDGE_MQ_STREAM`; `0.0.0.0` and
  the public port by default), so a laptop can run it next to a local Postgres.
- With the edge on, rolld points the products' own names at the edge's unless they're set:
  `ROLL_S3_DOMAIN`, `ROLL_S3_PUBLIC_ENDPOINT` (`https://s3.<region>.<site domain>`, the endpoint
  storage shows), Alternator's domain, `ROLL_CODE_PREVIEW_DOMAIN` (`roll.site`), roll code's
  public, git and web URLs; its own plain Kafka listener moves to `127.0.0.1:19092` if it would
  take the edge's port.
- Connection details name the public hostnames over TLS (`rolld/src/edge.rs`, `Public`), never a
  node's own listeners: nosql `<id>.nosql.<region>.<domain>:9042` with `tls: true` and the
  server name, Alternator `https://<id>.nosql.<region>.<domain>`, no shard-aware or REST port;
  streams `<stream id>.kafka.<region>.<domain>:9092` with `SASL_SSL` and
  `https://<stream id>.kafka.<region>.<domain>/v1/streams/<id>`; Postgres
  `<endpoint>.pg.<region>.<domain>:5432` with `sslmode=require`; Redis
  `rediss://…@<id>.redis.<region>.<domain>:6379` ("Redis on the fleet"); queues
  `amqps://…@<vhost>.mq.<region>.<domain>:5671/<vhost>`, `mqtts://…:8883`,
  `stomp+ssl://…:61614/<vhost>`, `rabbitmq-stream+tls://…:5551/<vhost>` and
  `https://<vhost>.mq.<region>.<domain>`, with no plain AMQP and no management port (the overview's
  `endpoints` are the default vhost's). Tested by `rolld/tests/edge.rs`, which also runs a stream
  client over TLS through the edge.

## Deploys, scaling and failure

- **Pipeline** (`.github/workflows/deploy.yml`): every merge to `main` builds one arm64 image
  (rolld plus the dashboard), pushes it to ECR through GitHub OIDC (no stored AWS keys), then:
  1. **canary**: one node of the new version joins the fleet and takes its share; it bakes for
     10 minutes while alarms watch errors (HTTP 5xx), p99 latency, failed lease handoffs, lease
     takeovers, TCP resets, crashed or unhealthy nodes and the healthy-node count ("Metrics");
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

Code checks, previews and agent sandboxes run customer code. On the fleet each run is its own
Fargate task (ECS `RunTask`, a Firecracker microVM), with a task role that can do nothing and no
way into the fleet's network: customer code never runs on a fleet node or through a Docker
socket. One node and local development keep running checks and previews as Docker containers on
the host (`ROLL_CODE_RUNNER=docker`, the default), exactly as before.

- **One seam.** `code_preview::Isolation` (`crates/code-preview/src/isolation.rs`) prepares a
  job's container with its tree, runs a check to its exit with its output, starts a preview and
  says where it answers, and removes everything of a run by name. `Docker` (the `docker` CLI) and
  `Fargate` (`fargate.rs`, aws-sdk-ecs) implement it; the callers keep the guarantees: a fork's
  code waits for `approve-run`, a check's timeout covers its run and then everything it made is
  removed (again a minute later, for a `RunTask` cut short), and the tree comes from
  `Code::checkout`, only read while it's packed. Sandboxes keep their own seam
  (`SandboxBackend`): `code_preview::sandbox::FargateSandboxes` is its Fargate backend
  (`ROLL_SANDBOX_BACKEND=fargate`). With `ROLL_CODE_RUNNER=fargate` rolld refuses the docker
  sandbox backend.
- **A task** is three containers from a template task definition in the stack (`<stack>-check`,
  `-preview`, `-sandbox`; arm64): `fetch` and `guard` run roll's small image (`roll-fetch`,
  `crates/roll-fetch`, pushed as `fetch-<tag>`), `run` is the customer's image. The image is the
  one thing `RunTask` can't override, so rolld registers a copy of the template per image (family
  `<template>-<hash of template and image>`), once, and reuses it. CPU and memory are task
  overrides rounded up to a Fargate size. Checks run on Fargate Spot when `ROLL_CODE_FARGATE_SPOT=1`
  (one that's interrupted runs again on demand); previews and sandboxes on demand.
- **The tree goes in as a bundle**: rolld packs the run's env and tree (`roll_fetch::pack`, a
  gzipped tar, symlinks as symlinks, bounded by code-git's `Limits`: 100 MiB a file, 2 GiB in
  all) to `s3://<bucket>/code/runs/<run>.tgz` and gives `fetch` a presigned GET that lasts 15
  minutes. `fetch` downloads it and unpacks it into the task's own storage with
  `Store::materialize`'s rules (`roll_fetch::unpack`): plain relative paths only, nothing that
  exists is written again (a repeated name fails the run), nothing is written through a symlink,
  no hard links or devices, sizes bounded by the bytes read. `run` starts only once `fetch`
  succeeded, as `sh -c <run>` after sourcing the env. rolld deletes the object when the run ends
  (a preview's once its tree is in); the bucket expires anything under `code/runs/` after a day,
  noncurrent versions too.
- **Nothing secret is in a `RunTask` request** (it shows in `DescribeTasks` and CloudTrail): the
  env (data URLs with their passwords, a sandbox's machine token) is only in the bundle. The
  presigned URL in the request opens the bundle until it's deleted or expires.
- **Every task ends by itself.** A check's `guard` exits at its deadline (timeout + 11 minutes),
  which stops the task; a preview's `guard` is its proxy and exits once no byte has gone either
  way for the preview's idle time plus two minutes. So a node that dies mid-run leaves nothing
  running for long, and nothing scans for leftovers at startup.
- **Previews.** The repo's owner starts the task on the first visit, waits for it to run and for
  `fetch` to finish, and proxies `chg-<id>.roll.site` to the task's private IP on the preview port
  (`ROLL_CODE_FARGATE_PREVIEW_PORT`, 9970), where `guard` forwards to the app's port (`roll.toml`
  `port`; 9970 itself is refused). The address lives only in the owner's memory, with the rest of
  a running preview; nothing persists it, because a new owner never adopts a running task. A
  preview stops when it's idle (the owner's reaper, or its own guard), when its change lands or
  closes, and when its repo moves: the old owner stops it (if it's alive to), the new one marks
  previews stored as running asleep, and the next visit starts one there, stopping any older task
  of the preview's name first; one nobody visits again stops by itself once idle
  (docs/code/cluster.md, "Moving work"). Data URLs reach the repo's Postgres and Redis branches by
  their public names on the edge.
- **Sandboxes.** A running sandbox is one task named `roll-sbx-<id>` (`startedBy`), in group
  `roll-sandbox`. Asleep or stopped means no task, so waking needs its env again: the backend
  keeps it sealed with `ROLL_SECRET_KEY` at `code/sandboxes/<id>.sealed` until the sandbox is
  deleted, and hands it over in a bundle on each start; `fetch` writes it to `/roll/env` for the
  sandbox's user, who removes it once read.
- **The network** (`deploy/cloud/aws/roll-cloud.yaml`): tasks run in subnets of their own
  (`10.80.64.0/18`) whose network ACL refuses everything to and from the VPC except fleet nodes
  reaching a preview on its port (and the replies to the nodes' ephemeral ports), and in
  `IsolatedSecurityGroup`, which admits only nodes on the preview port. No node security group
  admits it. There's no NAT, so tasks get a public IP (`ROLL_CODE_FARGATE_PUBLIC_IP=1`): their
  only way out, to pull images and packages and reach roll's public endpoints like anyone else.
- **Roles.** The task role has no policy. The execution role pulls images and writes logs
  (`/<stack>/runs`, 7 days). The fleet's task role may `RunTask` only the runner families, in this
  cluster, `StopTask`/`DescribeTasks`/`ListTasks` in this cluster, register and describe task
  definitions, pass only the two runner roles to ECS, and read the runner logs (a check's summary
  is the tail of its `run` log).
- **Tests**: `code-preview/tests/fargate.rs` runs checks, a preview and a sandbox against a fake
  ECS and CloudWatch Logs API and S3 (in process, or MinIO with `ROLL_E2E=1`, which checks the
  presigned URLs' signatures), "running" each task the way Fargate would; the hostile bundles are
  in `roll-fetch/tests/hostile.rs` (docs/code/test-coverage.md, "Checks and previews").
- **Not done yet**:
  - Not run against AWS.
  - A sandbox's home lives in the task's storage, so a sleep loses it: provider logins and the
    daemon's `agent.db` start over on each wake (the relay's mirror keeps the records). An EFS
    access point per sandbox would keep it. Endpoints and API keys in the sandbox's env survive.
  - Only public images (no registry credentials), and they need an arm64 variant. Docker Hub's
    anonymous pull limits apply per task address.
  - A check's output is read from CloudWatch Logs once it ends, not streamed.
  - The `fetch` image comes from the stack's `ImageTag`, which changes on stack updates, not on
    every deploy: the bundle format (`roll-bundle-1`) must stay readable by the previous
    `roll-fetch`.

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
- **The operator token.** `ROLL_ADMIN_TOKEN` (`Authorization: Bearer`, 32 characters or more) opens
  every route rolld's API serves. One check (`crates/roll-auth`) sits in front of all the merged
  routers, and `crates/rolld/src/auth.rs` (`RULES`) is the one list of routes that don't need it:
  - public: the health endpoints (`/healthz` and each product's), `GET /v1/info` (the dashboard's
    sidebar reads it before sign-in) and storage's console sign-in; a product that's off
    answers 404 before the check;
  - the product decides: roll code (`/v1/code`, `/git`: owners, tokens, public repos, signed
    webhooks, device login), signed internal hops (`/internal`);
  - the token or the product's own credential: storage (console sessions), tasks (its token
    and API keys), streams' HTTP data plane and Confluent REST (stream API keys).

  Every other route, and any route a product adds later, is the operator's, on the internal API
  and through the edge (the API host, and the Kafka HTTP and tasks hosts, which carry control-plane
  routes too; Redis REST, S3 and Alternator are data planes with their own credentials). The token is
  compared in constant time and never logged; products get only an `Operator` marker on the
  request. `RUST_LOG=rolld::auth=debug` lists every route with its access at startup, and
  `crates/rolld/tests/auth.rs` walks that list.
- **Secure by default.** rolld refuses to start when it listens on any non-loopback address, or
  with `ROLL_EDGE=1`, without `ROLL_ADMIN_TOKEN`. On loopback it starts open, as before, and says
  so. With a token, tasks requires a token or API key and streams' HTTP data plane requires an
  API key (`ROLL_STREAM_REQUIRE_AUTH`); queues refuse the default `guest`/`guest` login off
  loopback.
- **CORS.** One policy for every control-plane API: the origins in `ROLL_DASHBOARD_ORIGIN`
  (comma separated, `https://roll.sh` in the cloud); on loopback, by default, the local
  dashboard (any port on localhost, 127.0.0.1 or [::1]). Off loopback without the setting, no
  browser origin is allowed. No cookies are involved: the dashboard sends tokens.
- **The dashboard** signs in with the operator token (or a roll code owner token) in the header
  of every page; it stays in the browser's localStorage and goes to every product's API only
  when roll code says it's the operator's (`GET /v1/code` `auth.operator`). Nothing is built
  into the bundle. Its server functions call rolld anonymously (public pages). `roll` sends
  `ROLL_ADMIN_TOKEN` too.
- Secrets live in Secrets Manager and are sealed with KMS; nodes get them through their task
  role. Internal hops are signed with the cluster secret and stay on the VPC.
- Security groups admit the NLB and the other nodes only; Cloudflare's proxied hostnames reach
  the origin with authenticated origin pulls.
- Customer code (checks, previews, sandboxes) runs in Fargate tasks with no AWS permissions, in
  subnets and a security group that can't reach the nodes ("Isolated work").

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
| `ROLL_SYSTEM_DATABASE` | the system database's URL | catalogs and accounts for every product; roll code's data unless `ROLL_CODE_DATABASE` is set; roll streams' catalog (schema `streams`) |
| `ROLL_STREAM_ACK_LINGER_MS` | `50` | how long a stream's owner gathers appends before writing its write-ahead log object: `acks=all` latency against S3 PUTs (at most one per busy stream per round) |
| `ROLL_PRODUCTS` | `redis,streams,tasks,storage,nosql,queues,code` | the products the node runs (see "Products"); Postgres is off until its engine runs in process |
| `ROLL_S3_BUCKET`, `AWS_REGION` | `roll-cloud-data-<account>`, `us-east-1` | all product data, under per-product prefixes |
| `ROLL_SECRET_KEY` | from Secrets Manager | seals owners' keys and the stored certificates |
| `ROLL_ADMIN_TOKEN` | from Secrets Manager | the operator token every control-plane API accepts |
| `ROLL_DASHBOARD_ORIGIN` | `https://roll.sh` | the only origin CORS allows |
| `ROLL_EDGE` | `1` | public listeners with TLS, SNI routing and PROXY protocol |
| `ROLL_EDGE_DOMAIN`, `ROLL_EDGE_SITE_DOMAIN`, `ROLL_EDGE_REGION` | `roll.sh`, `roll.site`, `us-east-1` | hostnames are built from these |
| `ROLL_EDGE_PROXY_PROTOCOL` | `1` | expect PROXY protocol v2 from the NLB |
| `ROLL_ACME_DIRECTORY`, `ROLL_ACME_EMAIL` | Let's Encrypt production, ops address | |
| `ROLL_CLOUDFLARE_API_TOKEN` | from Secrets Manager | DNS edit on both zones, for DNS-01 only |
| `ROLL_DASHBOARD_UPSTREAM` | `http://127.0.0.1:3000` | the dashboard container in the same task |
| `ROLL_EDGE_DRAIN_DELAY` | `15` | seconds the public listeners keep serving after `/healthz` starts failing |
| `ROLL_EDGE_ORIGIN_PULL` | `1` (the default outside dev mode) | Cloudflare's origin-pull certificate required on proxied hostnames |
| `ROLL_DRAIN_TIMEOUT` | `90` | seconds a stopping node waits for its leases to move; `ROLL_CODE_DRAIN_TIMEOUT` stays an alias |
| `ROLL_LOG_FORMAT` | `json` | one JSON object per line, with access lines and metrics (see "Logs") |
| `ROLL_QUEUES_USER`, `ROLL_QUEUES_PASSWORD` | `roll-admin`, generated in Secrets Manager | roll queues' admin login (guest/guest is refused behind the edge) |
| `ROLL_CODE_PREVIEW_DOMAIN` | `roll.site` | previews answer on `chg-<id>.roll.site` |
| `ROLL_CODE_RUNNER` | `fargate` | where checks and previews run: `docker` (this host, the default) or `fargate` ("Isolated work") |
| `ROLL_CODE_RUNNER_CONCURRENCY` | `16` | checks running at once from one node |
| `ROLL_CODE_FARGATE_CLUSTER`, `_SUBNETS`, `_SECURITY_GROUP` | the stack's cluster, isolated subnets (comma separated) and `IsolatedSecurityGroup` | where tasks run |
| `ROLL_CODE_FARGATE_CHECK_TASK`, `_PREVIEW_TASK`, `_SANDBOX_TASK` | `<stack>-check`, `-preview`, `-sandbox` | the template task definitions |
| `ROLL_CODE_FARGATE_PUBLIC_IP` | `1` | a public IP per task: their only way out without a NAT gateway |
| `ROLL_CODE_FARGATE_SPOT` | `1` (`ChecksOnSpot`) | checks on Fargate Spot |
| `ROLL_CODE_FARGATE_PREVIEW_PORT` | `9970` | where nodes reach a preview task |
| `ROLL_CODE_FARGATE_BUCKET`, `_PREFIX` | `ROLL_S3_BUCKET`, `code/runs/` | where bundles go |
| `ROLL_SANDBOX_BACKEND` | `fargate` with a `SandboxImage`, else `none` | agent sandboxes as Fargate tasks |

Public ports (NLB listener → node port): 443 → 8443 (HTTPS: dashboard, API, git, S3, REST data
planes, WebSockets), 5432, 6379, 9092, 9042, 5671, 8883, 61614 and 5551, each to the same port on
the node. The NLB's health check is `GET http://<node>:7070/healthz`: 200 while the node serves and
isn't draining. It runs every 5 s and takes a node out after 2 failures, so a stopping node keeps
serving for at least 15 s after `/healthz` starts failing before it closes listeners. The ECS stop
timeout is 120 s (drain 90 + NLB 10–15 + margin), which also fits inside Spot's two-minute notice.

The instances are the NLB's targets (an ECS service attaches at most five target groups; there are
nine ports), registered by the instance group. Client IP preservation is off: client addresses
come only from PROXY protocol v2, on every public port. Health checks on 7070 carry no PROXY
header.

### Products

`ROLL_PRODUCTS` (`--products`) is a comma list of `postgres`, `redis`, `streams`, `tasks`,
`storage`, `nosql`, `queues` and `code`; unset, every product runs. The older switches are
shorthands over the same set: `--no-storage`, `--no-queues` and `--no-nosql` take one out, and
`--code-only`, `--queues-only` and `--nosql-only` mean `ROLL_PRODUCTS=<that one>`
(`crates/rolld/src/products.rs` decides).

- A product that's off binds none of its ports (the edge's too), starts no background work, and
  its API routes answer 404 with `{"error": "roll <product> is off on this node (ROLL_PRODUCTS)",
  "product": …}`. Its hostnames on the edge answer 404.
- With Postgres off rolld never runs `docker` for computes. roll code's checks and previews
  run as Fargate tasks on the fleet (`ROLL_CODE_RUNNER=fargate`, "Isolated work"); elsewhere
  they use Docker on the host, and without it they fail while the node keeps serving.
- `GET /v1/info` (no token) answers `{"version", "node", "products"}`; the dashboard hides the
  products a node doesn't run.
- Every node of the fleet must run the same set: requests are forwarded to a resource's owner,
  whichever node that is.

### Logs

rolld writes one JSON object per line on stdout (`ROLL_LOG_FORMAT=json`;
`crates/rolld/src/logs.rs`): `timestamp`, `level` (`"ERROR"`, `"WARN"`, `"INFO"`, …), `target`,
`msg` and the event's fields. The stack's one metric filter counts `ErrorLogs` from
`{ $.level = "ERROR" }`.

Every HTTP request rolld answers, on the internal API (`listener: "api"`) or the edge's HTTPS
listener (`"https"`), gets one access line (`target: "access"`, `msg: "request"`) when its
response body ends:

| field | |
| --- | --- |
| `method` | |
| `route` | the matched route pattern (`/v1/code/repos/{owner}/{repo}`), `unmatched`, or what the edge served (`dashboard`, `queues`, `edge`). Never the path or query: no tenant names, tokens or query strings |
| `product` | the product the route belongs to (`git` for git's smart-HTTP routes), `dashboard`, `edge`, or `rolld` (`/v1/info`, unmatched paths) |
| `status` | a number |
| `duration_ms` | until the response body ended |
| `ttfb_ms` | until the response head was sent |
| `bytes` | response body bytes |
| `forwarded` | `true` when another node forwarded the request here (it logged it too) |

No header is logged (`Authorization`, `Cookie` and the rest). Health checks (`/healthz`) get no access line.

### Metrics

Access lines are also CloudWatch Embedded Metric Format records (`_aws`), so CloudWatch Logs
turns them into metrics without an API call from the node. Namespace `RollCloud`:

| metric | unit | dimensions | from |
| --- | --- | --- | --- |
| `Latency` | Milliseconds | `product`; `service` = `rolld` | `ttfb_ms` of each request (p50/p99 are CloudWatch statistics) |
| `Requests` | Count | `product`; `service` | 1 per request |
| `Http5xx` | Count | `product`; `service` | 1 per 5xx answer |
| `Handoffs` | Count | `service` | leases this node handed to another (per minute, and after a drain) |
| `HandoffFailures` | Count | `service` | handoffs that timed out with requests in flight, whose release didn't reach the backend, or that a drain left behind |
| `LeaseTakeovers` | Count | `service` | leases taken from a holder that never released them (it died or was cut off) |
| `LeasesFenced` | Count | `service` | leases this node stopped using because it couldn't renew them in time |

Forwarded requests carry no metrics: the node that received them counted them. The lease
counters come from `roll_cluster::Cluster::stats` (roll-cluster has no AWS code).

Alarms (`deploy/cloud/README.md`, "Alarms"): `errors` on `Http5xx`; `latency` on the slowest
p99 of code, postgres, redis, storage, nosql and tasks (streams and queues long-poll, and git
pushes take as long as the pack); `handoff-failures` and `lease-takeovers` on any.

## Redis on the fleet

`crates/redis-control/src/cluster.rs` on top of `redis-server` (`node.rs`, `persist.rs`,
`preauth.rs`); rolld's part is `crates/rolld/src/redis.rs`. On the memory backend none of it runs.

- **Ownership.** A database is served by the holder of `redis:db:<id>`, a read replica by the
  holder of `redis:db:<primary>#<replica>`, which roll-cluster places as a standby of the primary
  (another node, another zone when one is live), so replicas serve reads away from it. The owner
  holds a guard while the database is loaded; a lease for a database that isn't loaded is given
  back after 30 s.
- **Routing.** Any node takes any connection. With a server name (the edge's 6379) the database
  is its first label, and the connection is spliced (`roll_edge::splice`) to the owner right after
  the TLS handshake. Without one, what comes before the first AUTH (or HELLO … AUTH) is answered as
  an unauthenticated server answers it (`-NOAUTH`), and the connection is spliced from that AUTH
  on. A database that is moving is waited for (a lease and its margin, 13 s); a connection that
  reaches a node the database just left is passed on (three splices at most). REST
  (`https://<id>.redis.…`, rolld's REST port) and the control routes
  (`/v1/redis/databases/{id}/…`, `/v1/redis/replicas/{id}`) are forwarded to the owner by
  `Cluster::dispatch`; forwarded REST arrives under `/internal/redis/rest`.
- **Catalog.** Databases, replicas, credentials and metering are in the system database
  (`ROLL_SYSTEM_DATABASE`, else `ROLL_CODE_DATABASE`; schema `redis`, through code-sql). A node
  that kept a `redis.db` of its own copies it in at start (what's there stays) and renames it
  `redis.db.imported`. One node keeps SQLite (`redis.db`) as before. Behind the edge, connection
  strings are `rediss://default:<id>_<secret>@<id>.redis.<region>.<domain>:6379` and
  `https://<id>.redis.<region>.<domain>`.
- **Fencing: storage decides, not the lease.** Each process writes a log of its own
  (`redis/seg/<node>.<random>/`), segment by segment, create-only. A node taking a database over
  first puts an empty object at the next segment number of the log the database's manifest names,
  then replays everything below it. The old owner's next segment write fails; it moves to a new
  log, and before a database's frames go into a log, its manifest is compare-and-swapped to name
  that log. A database whose manifest someone else wrote is dropped at once: its clients are
  disconnected, and a `sync` write waiting for durability is never acknowledged. Snapshot keys
  carry the writer's log. A manifest written on a clean unload names no log (nothing to fence).
- **Handoff** (a drain, rebalancing): the owner closes the database's connections over 2 s, each
  at its own moment, after answering the commands it already read; flushes everything and writes
  the final manifest; only then lets the lease go. Nothing acknowledged is lost, `async`
  durability included.
- **What a client sees on a handoff.** Its connection closes between two commands, without an
  error reply; a reconnect through any node reaches the new owner within about a second (one that
  lands on the old node in the instant it lets go is closed once more, without an answer). As in
  a Redis failover: a command already read is run and answered before the close; a MULTI not yet
  EXEC'd is discarded with the connection, never half run; a WATCH ends with the connection (the
  client runs its transaction again); a blocking command (BLPOP, XREAD BLOCK, WAIT) ends with the
  connection unserved; pub/sub subscriptions end, and the client subscribes again on reconnect
  (messages published meanwhile are missed, as on any Redis reconnect). No `-MOVED`: clients
  needn't speak Redis Cluster. `-LOADING` only if a database is still moving after 13 s. A REST
  request in flight finishes; new ones wait for the new owner.
- **On a crash** connections drop; reconnects wait while the dead owner's lease runs out (10–15 s),
  then find every write acknowledged under `sync` durability, and under `async` every write the
  owner had flushed (all but its last second at most).
- Garbage collection runs on one node at a time (singleton `redis-gc`).

## Testing the fleet

`pnpm test:fleet` (`scripts/test-fleet.sh`; the `fleet` workflow on GitHub, run by hand) is the end
to end test of this design. It builds the fleet image (dev profile), brings up a local copy of
roll cloud (`deploy/cloud/local/docker-compose.yml`) and runs `crates/rolld/tests/fleet_e2e.rs`
against it, then removes everything it created (all named `roll-cloud-e2e-*`).

- **The stack.** Three `rolld` nodes from the fleet image with the cloud's node settings
  (`ROLL_CLUSTER_BACKEND=dynamodb` on DynamoDB Local, `ROLL_SYSTEM_DATABASE` on
  `postgres:18-alpine`, S3 on MinIO, `ROLL_EDGE=1` with self-signed wildcard certificates for
  `roll.localhost` and `rollsite.localhost` from `deploy/cloud/local/gen-certs.sh`,
  `ROLL_PRODUCTS` without postgres, `ROLL_LOG_FORMAT=json`), and HAProxy in front as the NLB: TCP
  passthrough on every public port, PROXY protocol v2, `GET /healthz` on 7070 every second, out
  after two failures (the drain delay is 5 s to match). A fourth node is the scale-out
  (`--profile scale`). Ports are published on 127.0.0.1 and the test drives the stack from the
  host by the edge's hostnames (`*.localhost` resolves to loopback).
- **The load**, through the load balancer, with real clients, each writer recording every
  acknowledged write and a reader beside it: roll code (API landings on four repos, `git push` and
  fetches over `git.`), Redis (RESP over TLS with SNI, a `sync` database), NoSQL (CQL over TLS with
  SNI, a `sync` database), streams (librdkafka, SASL_SSL, `acks=all`, idempotent), storage
  (aws-sdk-s3, path and virtual-host style, multipart uploads), tasks (publish with `dedup_id`,
  pull and ack), queues (lapin over AMQPS with the URL the API gave a vhost, publisher confirms to
  a quorum queue, a consumer with manual acks).
- **The disruptions**: SIGKILL of the node holding the most leases (started again afterwards); a
  rolling restart, each node stopped with SIGTERM (it must exit 0 holding no lease) and healthy
  again before the next; a fourth node joining and leaving. After each, every tenant lease must be
  on the node placement prefers.
- **The checks**: every acknowledged write reads back; nothing is duplicated where the product
  promises exactly once (code landings, Kafka records, storage keys), tasks' at-least-once
  deliveries carry the message id the publish returned, and every confirmed queues publish is
  delivered (at least once, by message id); every client-visible error is a kind the
  product's docs allow (`Product::allows` lists them, with the docs they come from); no operation
  fails after the client retry policy. It prints writes, acknowledgements, verified writes,
  errors by kind and the longest stall, per product and per disruption.

## Where everything lives

| part | path |
| --- | --- |
| AWS stack | `deploy/cloud/aws/roll-cloud.yaml` |
| local fleet and its end to end test | `deploy/cloud/local/`, `crates/rolld/tests/fleet_e2e.rs`, `scripts/test-fleet.sh` |
| Cloudflare zones and records | `deploy/cloud/cloudflare/` |
| fleet image | `deploy/cloud/Dockerfile` |
| deploy pipeline | `.github/workflows/deploy.yml` |
| cluster layer | `crates/roll-cluster`; rolld's settings and handle in `crates/rolld/src/cluster.rs` |
| edge (TLS, SNI, ACME, PROXY protocol, routing) | `crates/roll-edge` |

## Status

| part | state |
| --- | --- |
| code + agents on the cluster layer | built: `crates/roll-cluster` with Memory, Postgres and DynamoDB backends; `ROLL_CLUSTER_*`, `ROLL_NODE_ID`, `ROLL_ZONE`, `ROLL_ADVERTISE_URL`, `ROLL_DRAIN_TIMEOUT` in rolld |
| AWS stack, image, pipeline, Cloudflare | built (`deploy/cloud/README.md`); not yet run against AWS |
| edge | built (`crates/roll-edge`, `rolld/src/edge.rs`); not yet run against AWS |
| control-plane auth lockdown | built (`ROLL_ADMIN_TOKEN`, `crates/roll-auth`, `rolld/src/auth.rs`); customer accounts next |
| product switches, JSON logs, metrics and their alarms | built (`ROLL_PRODUCTS`, `ROLL_LOG_FORMAT`, `GET /v1/info`); not yet run against AWS |
| GitHub resilience | done |
| storage on the cluster layer | built (`storage:meta`, `s3-server/src/cluster.rs`); per-account logs designed |
| tasks on the cluster layer | built (`tasks:cell:default`, `tasks/src/fleet.rs`); tenants per node designed |
| redis on the cluster layer | built ("Redis on the fleet"); not yet run against AWS |
| nosql on the cluster layer | built: lease `nosql:db:<id>`, catalog on the system database, CQL spliced to the owner (SNI or SASL user), fenced manifests (docs/nosql/architecture.md, "On a fleet") |
| streams on the cluster layer | built (`stream:cluster:<id>`, catalog on the system database, acknowledgements from a write-ahead log in S3, kafka splices); not yet run against AWS |
| queues on the cluster layer | built: lease `mq:<vhost>/<queue>`, metadata on the system database (schema `mq`), logs in S3 Standard with group commit (no keepers, no proxies), drain = roll-cluster's (docs/queues/deploy-aws.md) |
| fleet end to end test | built (`pnpm test:fleet`, "Testing the fleet"): every product but postgres through a SIGKILL, a rolling restart and a scale-out, through HAProxy with PROXY v2 |
| postgres on the fleet and the system database | with the pg-engine branch |
| isolated work on Fargate | built ("Isolated work": checks, previews and sandboxes as Fargate tasks, `crates/roll-fetch`, the stack's isolated subnets and runner templates); not yet run against AWS |

### Open items

- **Postgres on the fleet.** Today's Postgres product starts computes with the `docker` CLI, so
  the stack runs with it off (`ROLL_PRODUCTS`) until the in-process engine replaces them.
- **Isolated work** hasn't run against AWS, and a Fargate sandbox's home doesn't survive a sleep
  ("Isolated work", "Not done yet").
- **Buckets on roll.site** are served by host (`<bucket>.roll.site/<key>`, the Worker's
  `ROLL_BUCKET_DOMAIN`) and by path on the zone (`roll.site/<bucket>/<key>`); set the deployment's
  CDN `base_url` to `https://roll.site` so purges reach both. Bucket names starting `chg-` are
  reserved for previews.
- **Queues' confirm latency.** Queue logs are S3 Standard objects, group-committed per node: a
  publisher confirm waits about 73 ms p50 (240 ms p99) with S3's latency modelled at 25 ± 10 ms per
  request (docs/benchmarks/queues-fleet.md), so one queue's throughput is its confirm window over
  that. Keepers embedded in the nodes (a 2-of-3 quorum across zones on local disks) would confirm
  in about 2 ms, but need dynamic quorum membership and re-replication as nodes come and go
  (docs/queues/storage.md, "On roll cloud"). Pipelining the object log's appends would bring S3
  towards one PUT per confirm. Not yet measured against real S3.
- **Single active consumer across nodes.** Stream protocol single-active-consumer groups are
  coordinated by each node's stream server. Behind the edge a group's consumers reach whichever
  nodes the NLB picks, and consumers of one group on different nodes are each active. It needs
  the group kept at the stream's owner (the router could carry registrations and
  `ConsumerUpdate`s as it carries subscriptions). Off the edge clients connect to the owner, and
  groups work.
- **code-control `automations::a_run_starts_an_agent_waits_for_it_and_notifies`** failed once on
  GitHub under load and never locally; its helpers now wait up to 60 s for the state they check
  (they had 5–20 s), and reading the engine, relay and event stream found no race. If it fails
  again, keep the panic message: it names the helper that gave up.
- **Per-table tombstone GC switches** (`/column_family/tombstone_gc/<ks>:<cf>`,
  `/storage_service/tombstone_gc/<ks>`) are answered without doing anything in roll nosql, as the
  per-table autocompaction switch was until it broke a conformance test.
- **Images not pinned by digest**: the Postgres compute images (`postgres:{16,17,18}-alpine`, one
  `{v}` template in `pg-control`'s config that the gates' pre-pulls must match) and the bases of
  the `deploy/` Dockerfiles that end-to-end gates build (`deploy/code`, `deploy/queues`,
  `deploy/nosql`). Every other image a suite or gate pulls is `tag@sha256:<multi-arch index>`.
- **What the fleet test showed** (`pnpm test:fleet`, 2026-11-08; "Testing the fleet"). Each run
  makes about 4.5k code landings, 6.5k Redis `SET`s, 6k CQL `INSERT`s, 21k Kafka records, 3k
  objects and 1k–2k tasks publishes through a SIGKILL, a rolling restart and a scale-out. Every
  drained node exited 0 holding no lease, and placement was right after every scenario. It found:
  a lost acknowledged tasks publish (a dedup claim committed apart from its message: fixed with
  `Store::write_all`, and the tasks consumers that fell behind a backlog: fixed by committing a
  pull once), takeover after a crash at about 10 s, a stopping node waiting for the load balancer
  twice, and NoSQL and streams handing out internal endpoints behind the edge (all three fixed by
  lane `cloud/hardening`), and an image that didn't build. Still open from it:
  - **Streams stall the longest on a planned handoff**: a producer waited 2.7–6.1 s for
    acknowledgements during the rolling restart (Redis and NoSQL at most 2.6 s, others under
    1.5 s); each move costs librdkafka a reconnect and a metadata refresh.
  - The takeover and stop-order numbers above predate the hardening fixes: rerun the fleet test to
    measure them again.
