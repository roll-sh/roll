# Deploy roll search on AWS

`deploy/search/aws/roll-search.yaml` is a CloudFormation stack for a production fleet that
serves many Elasticsearch-compatible clusters and costs almost nothing while they are idle:

```
 clients ──► Lambda function URL ─► internal NLB          (default)
         └─► NLB with TLS (CertificateArn, Domain)        (high volume, large bulks)
               │  cross-zone off: a flow stays in its AZ
               ▼
 search-proxy (Fargate arm64, ¼ vCPU) ×3 ── routes each request to the node owning its cluster
               │                            (rendezvous hashing, own AZ first); holds requests and
               │                            starts nodes (ECS UpdateService) when none run
               ▼
 search nodes: rolld --search-only (ECS on Graviton: on-demand base + Spot, or Fargate) ×N
   │ catalog, ownership leases, node records          │ WAL group commits, segments, manifests
   ▼                                                  ▼
 Postgres (RDS in the stack, or roll postgres)     S3 Standard (+ S3 Express One Zone for the WAL)
```

- **Nodes are stateless.** Local disk is a cache. A cluster is served by the node holding its
  ownership lease (an epoch in Postgres); the epoch is written into every WAL batch and
  manifest and an ownership marker in S3, so a deposed owner can't commit. Writes are
  acknowledged once the WAL batch is in object storage (`ROLL_SEARCH_SYNC_OBJECTS`, always on in
  a fleet), so losing a node loses no acknowledged write.
- **Proxies** are the only thing the load balancer talks to. They read the node records from
  Postgres (one row per node, heartbeated every second), send each request to its cluster's
  owner and follow a node's `X-Roll-Search-Owner` refusal when ownership moved. They hold
  requests while no node runs, while a cluster moves (deploys, Spot reclaims) and while a dead
  owner's lease expires, and resend a request only when the node provably didn't run it, or
  when it is read-only. Request bodies stream through and are kept as they pass (up to 100 MiB
  per request, `ROLL_SEARCH_PROXY_RETRY_BODY_MB`, and 128 MiB per proxy,
  `ROLL_SEARCH_PROXY_RETRY_BUFFER_MB`), so a `_bulk` refused mid-upload is resent whole to the
  owner. Only a node's own failures count against it: a client that hangs up mid-upload gets
  nothing resent, only a failed connect takes a node out of rotation (a broken connection is
  something a client can provoke), a read whose connection broke is resent once to another node,
  and only answers past a node's credential check (not a 401 or 403) teach a proxy who owns a
  cluster.
- **Every hop is TLS.** Clients reach the front door over TLS (the function URL, or the NLB
  with your certificate), and so is everything behind it:
  - *Proxy to node* (`NodeTls`, on by default). Each node makes a self-signed certificate at
    boot (`ROLL_SEARCH_TLS_SELF_SIGNED`) and publishes its SHA-256 in its node record; proxies
    accept exactly that certificate from that address, so there is no CA or certificate to
    rotate, and the trust comes from the (TLS-verified) metadata database. A `--search-only`
    node serves its control API port (7070) with the same certificate, so the API calls the
    proxies send and those nodes forward to each other (a console request for a cluster
    another node holds) are pinned the same way. A node started with its own certificate
    (`ROLL_SEARCH_TLS_CERT`/`_KEY`) publishes that one's pin. Node records without a pin are
    reached over plain HTTP.
  - *Front door to proxy* (`FrontTls`, on by default). With the NLB, its target groups are TLS
    and each proxy serves a certificate made at boot (`ROLL_SEARCH_PROXY_TLS_SELF_SIGNED`; an
    NLB encrypts to targets without verifying them). With the function URL, the internal NLB
    passes TCP through and the function itself speaks TLS to the proxies, accepting only the
    certificate they derive from the stack secret `ProxyTlsSeed` (`ROLL_SEARCH_PROXY_TLS_SEED`
    and `ROLL_SEARCH_LAMBDA_TLS_SEED`): an Ed25519 certificate is fully determined by its key,
    so every proxy and the function arrive at the same one. CloudFormation resolves the seed
    into the function's configuration (without a NAT the function can't call Secrets
    Manager), so whoever may read that configuration can read the seed.
  - *Node to Postgres*: verified TLS (see `DatabaseMode`). *Node to S3*: HTTPS through the
    gateway endpoint.
- **Clusters sleep, nodes scale.** A cluster without requests for `IdleClusterSeconds` unloads
  (flushed, nothing of it in memory) and wakes on its next request in milliseconds on any node.
  The fleet scales out on CPU and in one node at a time; with `MinNodes=0` the last node stops
  after `IdleMinutesToZero` minutes without any request, and the next request starts one.
- **No NAT gateway.** The stack has its own VPC with public subnets; S3 and S3 Express go
  through free gateway endpoints. Nodes and the database have no public address; node instances
  and Fargate tasks use a public address only to pull the image, write logs and (proxies) call
  the ECS API. Security groups admit nothing from outside except the load balancer's ports.

## Deploy

1. Build the image for arm64 and push it to ECR. With the Lambda front door the tag must be a
   single-architecture image (Lambda rejects image indexes, including the one attestations
   create):

   ```sh
   aws ecr create-repository --repository-name roll-search
   docker buildx build --platform linux/arm64 --provenance=false -f deploy/search/Dockerfile \
     -t <account>.dkr.ecr.<region>.amazonaws.com/roll-search:latest --push .
   ```

   The image contains `rolld` and `search-proxy` (which is also the Lambda runtime). The
   build stage runs on the builder's own platform and cross-compiles (gcc cross toolchains for
   the bundled C), and the runtime stage runs no commands, so an x86 machine builds the arm64
   image at native speed without QEMU (about 15 minutes for a release build on 6 jobs). For a
   multi-arch tag (ECS only) pass `--platform linux/arm64,linux/amd64`.

2. Deploy the stack (it creates its VPC in the region's first three AZs):

   ```sh
   aws cloudformation deploy --stack-name roll-search \
     --template-file deploy/search/aws/roll-search.yaml --capabilities CAPABILITY_IAM \
     --parameter-overrides ImageUri=<account>.dkr.ecr.<region>.amazonaws.com/roll-search:latest
   ```

   That is the cheapest shape: a Lambda function URL in front, one on-demand Graviton node, RDS
   `db.t4g.micro` for metadata. For a high-volume endpoint add
   `CertificateArn=arn:aws:acm:… Domain=search.example.com` (and `ClusterHostNames=true` with a
   wildcard certificate for `<cluster>.search.example.com`), then point the domain at the
   `LoadBalancer` output.

3. Create clusters through the control API (`ApiUrl` output; the token is the
   `ApiTokenSecret` value):

   ```sh
   export ROLL_API_URL=$(aws cloudformation describe-stacks --stack-name roll-search \
     --query "Stacks[0].Outputs[?OutputKey=='ApiUrl'].OutputValue" --output text)
   export ROLL_API_KEY=$(aws secretsmanager get-secret-value --secret-id <ApiTokenSecret> \
     --query SecretString --output text)
   roll search clusters create logs
   ```

   The answer has the cluster's URL (the `Endpoint` output, or `https://logs.<Domain>`), user
   and password. Any Elasticsearch client works with them:

   ```sh
   curl -u logs:<password> https://<endpoint>/_cluster/health
   ```

   The dashboard (`/dashboard/search`) uses the same API: build or run it with
   `NEXT_PUBLIC_ROLL_SEARCH_API` set to the `ApiUrl` and `NEXT_PUBLIC_ROLL_SEARCH_TOKEN` to the
   token (it is compiled into the browser bundle, so only hand that build to people who may
   hold the token). The API answers CORS preflights, so a dashboard on your laptop works.

| parameter | default | |
| --- | --- | --- |
| `CertificateArn` / `Domain` | empty | Empty: a Lambda function URL in front (HTTPS on an AWS domain, about $0.40 per million requests with duration, 6 MB request and response limit). Set: an internet-facing NLB with TLS 1.3/1.2 on 443 (Elasticsearch REST) and 8443 (control API), about $20/month with no per-request charge and no payload limit; cheaper above roughly 50 million requests a month, and the only choice for bulks over 6 MB. |
| `ClusterHostNames` | `false` | With the NLB, give clusters their own host names (`<cluster>.<Domain>`, SNI/`Host` routing; needs a wildcard certificate). Otherwise clusters share the endpoint and are told apart by their credentials, which every client sends anyway. |
| `ClientCidr` | `0.0.0.0/0` | Who may reach the NLB. |
| `NodeCompute` | `ec2` | `ec2`: Graviton instances from an Auto Scaling group behind an ECS capacity provider (`OnDemandNodes` on demand, the rest Spot, `price-capacity-optimized` over every current Graviton type of the size). `fargate`: Fargate for `OnDemandNodes`, Fargate Spot above; no instances, faster cold starts, higher price. |
| `MinNodes` / `MaxNodes` | 1 / 8 | Nodes always running, and the most there may be. `MinNodes=0` lets the fleet stop (see below). |
| `OnDemandNodes` | 1 | Nodes never on Spot. |
| `NodeVcpus` / `NodeInstanceMemoryMiB` | 2 / 8192 | With `ec2`, the instance size (m7g.large, m8g.large, r7g.large, …). |
| `NodeCpu` / `NodeMemory` | 2048 / 6144 | Per node task. Loaded clusters may hold three quarters of the memory together; past it the least recently used idle clusters unload. |
| `NodeCacheGiB` | 30 | Local disk per node (21–200): the segment cache, the WAL and segments being written; a tenth of it (at least 2 GiB) stays free. With `ec2`, a gp3 volume of its own mounted at `/var/lib/roll-search` and bound to the node's `/data` (the instance's 30 GiB root volume keeps the OS, the ECS agent and Docker, and one node runs per instance); the next node on an instance keeps its warm segment cache. With `fargate`, the task's ephemeral storage. |
| `CpuTarget` | 60 | Average CPU the fleet scales out to keep; scale-in removes one node at a time after 15 minutes below a third of it. |
| `IdleClusterSeconds` | 300 | A cluster without requests for this long unloads (a cluster's own `idle_timeout_s` wins; 0 there keeps it loaded). |
| `IdleMinutesToZero` | 15 | With `MinNodes=0`: no byte through the load balancer for this long stops the last node. |
| `ProxyCount` | 3 | Proxy tasks; ECS spreads them over the AZs. |
| `ExpressZoneId` | empty | An AZ id (e.g. `use1-az4`): keep the WAL in an S3 Express One Zone directory bucket there. Acks take single-digit milliseconds instead of S3 Standard's tens, and WAL requests cost about half. WAL batches not yet flushed into segments live in that AZ only. |
| `NodeTls` | `true` | Proxies (and nodes forwarding API calls) reach nodes' REST and API ports over TLS pinned to each node's boot-time certificate (see above). `false`: plain HTTP inside the VPC. |
| `FrontTls` | `true` | The NLB (TLS target groups) or the function (pinned to the `ProxyTlsSeed` certificate) reaches the proxies over TLS. `false`: plain TCP inside the VPC. |
| `DatabaseMode` | `rds` | `rds`: a PostgreSQL instance (`DatabaseInstanceClass`, `DatabaseEngineVersion`) in the stack, its password in a stack secret, TLS verified with the RDS CA bundle in the image. `external`: the URL in `MetadataUrlSecretArn` (`postgres://user:pw@host:5432/db?sslmode=require`; roll postgres or any Postgres 14+). With TLS the certificate chain and host name are always verified, so `require` behaves like libpq's `verify-full`; `verify-ca` and `verify-full` are accepted as synonyms. |

## Ownership, failover and handoff

- **Leases.** A node takes a cluster's lease (compare-and-set in Postgres, with an epoch) before
  it reads anything of it, renews every lease it holds in one statement every `ttl/6` (also
  during a long wake), and stops acknowledging writes `0.8 × ttl` after it sent its last
  successful renewal, so a node cut off from Postgres stops answering writes before anyone
  else may take over. An expired or released lease is never renewed. Every acknowledgement
  in a fleet also checks the cluster's newest ownership marker in S3, so even a lease store that
  wrongly vouches for a deposed owner can't make it acknowledge a write the new owner won't see.
- **Node loss** (crash, instance failure). Its heartbeat stops and proxies drop it after 5 s;
  its leases expire after the TTL (10 s). The next request for each of its clusters waits at the
  proxy, goes to the cluster's new rendezvous node, which takes the lease with a higher epoch
  and opens the cluster from its manifests plus the WAL after them. Nothing acknowledged is lost.
- **SIGTERM** (deploys, scale-in, Spot reclaims). The node publishes `draining` (proxies send it
  nothing new) and refuses every new request with `node_closed_exception` before running it;
  the proxy holds those and resends them to the next owner. The requests in flight finish, then
  each cluster flushes and hands its lease back at once, and the node leaves the fleet. A
  steady stream of requests can't starve the drain, so clusters move in about the time their
  flush takes. Only a request still running when the drain times out (100 s) is cut off: the
  cluster is then dropped without a flush and that request isn't acknowledged, since the lease
  is revoked before it is handed back. The whole stop is bounded by
  `ROLL_SEARCH_SHUTDOWN_TIMEOUT` (100 s): handing clusters over gets 85 s, and the last 15 s
  cover metering, leaving the fleet and the last connections finishing. The task stop timeout
  is 120 s, which leaves room inside Spot's two minutes for the time between the notice and
  ECS's SIGTERM.
- **Spot.** With `NodeCompute=ec2`, the ECS agent drains interrupted instances
  (`ECS_ENABLE_SPOT_INSTANCE_DRAINING`), capacity rebalancing replaces instances at risk before
  the notice, and the capacity provider protects instances running nodes from scale-in until
  their tasks have moved. Keep `OnDemandNodes` at 1 or more so a Spot shortage leaves the fleet
  smaller but up. Proxies never run on Spot.

## Scale to zero

With `MinNodes=0`, when the load balancer has carried no byte for `IdleMinutesToZero` minutes,
an alarm sets the node service to 0 tasks (each node drains first; the alarm breaches at exactly
its threshold, so its step covers `[0, ∞)`). The proxies stay up. The
alarm only counts minutes in which nodes run, so it returns to OK once the fleet is at zero and
can't stop the node a wake has just started (an alarm left in ALARM re-runs its scaling action
every minute, faster than the load balancer's metrics show the request that woke the fleet).

- The next request waits at a proxy, which scales the service back to 1 (`DescribeServices`,
  then `UpdateService`; it never lowers a count) and forwards as soon as a node heartbeats:
  typically 20–40 s on Fargate and about a minute on EC2, where an instance boots first. The
  proxies hold requests up to 120 s and the Lambda function waits 150 s, so give clients a
  request timeout above that (Elasticsearch clients default to 30–60 s; retries are safe for
  reads).
- Load balancer health checks go to the proxies' health port, which never touches nodes, so
  they don't wake the fleet and don't count as traffic.
- Clusters themselves sleep long before that: `IdleClusterSeconds` after their last request.
  A shared fleet usually keeps `MinNodes=1` and lets clusters sleep; stopping the nodes too
  pays off for small deployments idle for hours at a time.

## What it costs to run

The fixed part is three ¼-vCPU proxy tasks, one NLB (internal behind the function URL, or
internet-facing), the database (`db.t4g.micro` by default) and `MinNodes` nodes (one on demand,
the rest Spot). Idle clusters cost only their S3 storage. WAL batches are shared by every
cluster on a node, so S3 request costs follow nodes and traffic, not the number of clusters.
Cross-AZ transfer stays small: the load balancer and proxies are AZ-local, and proxies prefer
an owner in their own AZ when choosing where a cluster wakes.

A cluster's Usage tab in the dashboard (`/dashboard/search/<cluster>`) and `roll search clusters usage` show what each
cluster used (requests, stored GB-months, loaded GB-hours) and compare it with Elastic Cloud and
Amazon OpenSearch Service for the same workload. Check current AWS prices for your region before
sizing.

## Status

The template passes `cfn-lint` (run in Docker). It has not been deployed by our CI.

The pieces it runs are tested locally:
- the proxy against fake nodes: rendezvous routing and owner redirects, own AZ first, holding
  requests and waking the fleet from zero through a fake ECS endpoint, the hold timeout,
  retries across a shutdown handoff, request bodies refused mid-upload and resent whole (and
  bodies over the resend bounds teaching the client's retry the owner), dead and broken nodes
  (a broken read resent once), a client hanging up mid-upload marking no node bad, a 503 larger
  than a refusal reaching the client whole, bad credentials and control API answers never
  moving the owner cache, TLS with SNI routing plus the PROXY protocol, graceful shutdown
  (`crates/search-proxy/tests/proxy.rs`); TLS to nodes' REST and API ports accepting only the pinned
  certificate, and no request to a plain process at a pinned node's address or to a node
  presenting another certificate (`crates/search-proxy/src/upstream.rs`, `tests/proxy.rs`);
- the Lambda runtime against a fake Runtime API, and its TLS hop to a real proxy serving the
  seeded certificate (a proxy with another stack's certificate gets nothing)
  (`crates/search-proxy/src/lambda.rs`);
- a fleet of real in-process nodes behind the real proxy, sharing SQLite (or Postgres with
  `ROLL_SEARCH_TEST_PG`) and object storage (or S3 with `ROLL_E2E_S3_BUCKET`): one owner per
  cluster, a killed owner failing over without losing acknowledged writes, a deposed owner
  fenced, a SIGTERM handoff under load, changes and deletes reaching the owner, branches cut on
  the parent's owner, a chunked `_bulk` refused by a non-owner mid-upload and resent to the
  owner, nodes serving TLS on both ports reached through their pinned certificates (console
  requests forwarded node to node over it, the proxy's API port too), and the fleet woken from
  zero (`crates/search-control/tests/fleet.rs`);
- lease and marker fencing in `search-server` and `search-store`, a wake that outlasts the
  lease, and a drain under a steady stream of writes that hands over without losing an
  acknowledged write (`crates/search-server/tests/fencing.rs`);
- `rolld --search-only` processes behind the proxy, a TLS node (`ROLL_SEARCH_TLS_SELF_SIGNED`,
  its API port too) next to a plain one, the cluster created through the proxy's API port:
  SIGTERM handoff under load, SIGKILL failover to a new node
  (`crates/rolld/tests/search_fleet.rs`);
- SigV4 signing of the ECS calls, checked against AWS's documented example.
- the Lambda function's image configuration: it sets both `EntryPoint` (`search-proxy`) and
  `Command` (`--lambda`), since Lambda would otherwise pass the image's `CMD ["rolld"]` to the
  overridden entrypoint; a unit test in `crates/search-proxy/src/main.rs` reads the template and
  the Dockerfile and parses that command line.
- the image itself (`deploy/search/Dockerfile`): the linux/arm64 release build, cross-compiled
  on x86 (aarch64 binaries needing glibc 2.35 at most, 86 MB image; it can't run here without
  an arm64 host), and a native linux/amd64 `PROFILE=dev` build that was run: a node with a
  boot-time certificate (REST and API) behind `search-proxy --tls-self-signed`, both from the
  image, sharing SQLite metadata; a cluster created through the proxy's API port over TLS, a
  document indexed and counted through its REST port, the node as uid 10001, and a SIGTERM that
  drains the node and exits 0.

The fleet, fencing, durability and persistence tests also pass against an S3-compatible server
(SeaweedFS: `ROLL_E2E_S3_BUCKET`, `ROLL_E2E_S3_ENDPOINT`, `AWS_*` credentials) and against
Postgres 18 (`ROLL_SEARCH_TEST_PG`).
