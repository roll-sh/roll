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

## First deployment, step by step

Nothing of this has run on a real AWS account yet: everything that can be checked without one is
rehearsed locally (see [What was verified locally](#what-was-verified-locally)), and step 6 lists
what only AWS can show, with the command that shows it. Expect about an hour, most of it waiting
for RDS and the first image push. The default shape costs roughly $45 a month before traffic
(three ¼-vCPU Fargate proxies, an internal NLB, `db.t4g.micro`, one `m7g.large` on demand).

### 0. Before you start

- An AWS account, the AWS CLI v2 with credentials for it (`aws sts get-caller-identity`), and a
  region with Graviton instances, Lambda and S3 (any commercial region). The stack creates its
  own VPC in the region's first three AZs.
- Docker with buildx on the machine that builds the image (an x86 machine is fine).
- Quotas: Fargate on-demand vCPUs (≥ 2), EC2 "Running On-Demand Standard instances" vCPUs
  (≥ 2 × `NodeVcpus` × `MaxNodes` for deploys), "All Standard Spot Instance Requests" if
  `MaxNodes` > `OnDemandNodes`, and the Lambda concurrency limit (1000 by default).
- The `roll` CLI (`cargo build --release -p roll-cli`).

### 1. Rehearse locally (optional, recommended)

These need only Docker and take about 40 minutes together:

```sh
scripts/search-image-smoke.sh --build --install-binfmt amd64 arm64   # the images, as AWS runs them
scripts/search-image-smoke.sh --s3 amd64                             # the same with S3 (LocalStack)
IMAGE=roll-search scripts/search-aws-rehearsal.sh                    # user data, scaling model, LocalStack
docker run --rm -v "$PWD/deploy/search/aws:/t:ro" python:3.13-slim \
  sh -c 'pip install -q cfn-lint && cfn-lint /t/roll-search.yaml'
```

`--install-binfmt` registers qemu-user with the kernel (once per boot) so the arm64 image runs on
an x86 machine.

### 2. Build the image and push it to ECR

Lambda takes a single-architecture image without an attestation index, so push the arm64 image
with `--provenance=false`:

```sh
export AWS_REGION=us-east-1 ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
export REPO=$ACCOUNT.dkr.ecr.$AWS_REGION.amazonaws.com/roll-search
aws ecr create-repository --repository-name roll-search --image-scanning-configuration scanOnPush=true
aws ecr get-login-password | docker login --username AWS --password-stdin ${REPO%/*}
TAG=$(git rev-parse --short HEAD)
docker buildx build --platform linux/arm64 --provenance=false -f deploy/search/Dockerfile -t $REPO:$TAG --push .
```

The image contains `rolld` and `search-proxy` (which is also the Lambda runtime). The build stage
runs on the builder's own platform and cross-compiles (gcc cross toolchains for the bundled C),
and the runtime stage runs no commands, so an x86 machine builds the arm64 image at native speed
without QEMU (about 10 minutes for a release build on 8 jobs). Use a new tag per build: the
stack's deploys are driven by `ImageUri` changes. For a multi-arch tag (ECS only, not Lambda)
pass `--platform linux/arm64,linux/amd64`.

### 3. Deploy the stack

```sh
aws cloudformation deploy --stack-name roll-search \
  --template-file deploy/search/aws/roll-search.yaml --capabilities CAPABILITY_IAM \
  --parameter-overrides ImageUri=$REPO:$TAG
```

It takes 15–20 minutes (RDS is most of it). That is the cheapest shape: a Lambda function URL in
front, one on-demand Graviton node, RDS `db.t4g.micro` for metadata. For a high-volume endpoint
add `CertificateArn=arn:aws:acm:… Domain=search.example.com` (and `ClusterHostNames=true` with a
wildcard certificate for `<cluster>.search.example.com`), then point the domain at the
`LoadBalancer` output. If the stack fails, `aws cloudformation describe-stack-events --stack-name
roll-search --query "StackEvents[?contains(ResourceStatus,'FAILED')]"` names the resource; the
circuit breaker rolls a service back when its tasks don't start (their logs are in the
`/ecs/roll-search` log group).

### 4. Create a cluster and use it

```sh
out() { aws cloudformation describe-stacks --stack-name roll-search \
  --query "Stacks[0].Outputs[?OutputKey=='$1'].OutputValue" --output text; }
export ROLL_API_URL=$(out ApiUrl)
export ROLL_API_KEY=$(aws secretsmanager get-secret-value --secret-id "$(out ApiTokenSecret)" \
  --query SecretString --output text)
roll search clusters create logs           # prints the URL, user and password once
curl -u logs:<password> "$(out Endpoint)_cluster/health"
roll search request logs PUT /docs/_doc/1 '{"title": "hello"}'
roll search request logs GET '/docs/_search?q=title:hello'
roll search clusters usage logs
```

Any Elasticsearch client works with the endpoint, user and password. The dashboard
(`/dashboard/search`) uses the same API: build or run it with `NEXT_PUBLIC_ROLL_SEARCH_API` set to
the `ApiUrl` and `NEXT_PUBLIC_ROLL_SEARCH_TOKEN` to the token (it is compiled into the browser
bundle, so only hand that build to people who may hold the token). The API answers CORS
preflights, so a dashboard on your laptop works.

### 5. Look at it running

- Node and proxy logs: `aws logs tail /ecs/roll-search --follow` (a node logs `search
  listening … tls=true` and `search control API listening … tls=true`; a proxy logs `search
  proxy listening`). The function: `aws logs tail /aws/lambda/roll-search-front`.
- Services: `aws ecs describe-services --cluster "$(out Cluster)" --services "$(out NodeService)"
  --query 'services[0].[desiredCount,runningCount,deployments[].rolloutState]'`.

### 6. Check what only AWS can show

Each item below was rehearsed locally as far as possible (next section); these are the parts a
real account has to confirm, in the order worth doing them.

1. **The data volume on the node instances** (`NodeCompute=ec2`). On the instance (Session
   Manager: `aws ssm start-session --target <instance id>`): `findmnt /var/lib/roll-search` shows
   an xfs filesystem on an `nvme` device of `NodeCacheGiB` (gp3, encrypted), `ls -l /dev/xvdb`
   links to it, `grep roll-search /etc/fstab` has one `UUID=… nofail` entry, and
   `/var/log/cloud-init-output.log` shows the user data finished before `systemctl status ecs`
   started. An instance that found no volume powers off (`aws ec2 get-console-output
   --instance-id <id>` shows `roll search: no data volume`) and the Auto Scaling group replaces
   it. Also check the AMI's root snapshot still fits the 30 GiB root volume (`aws ec2
   describe-images --image-ids <ami>`; launches fail otherwise).
2. **The Lambda front door.** `aws lambda get-function-configuration --function-name
   roll-search-front --query ImageConfigResponse` shows `EntryPoint
   [/usr/local/bin/search-proxy]` and `Command [--lambda]`; step 4 going through the function URL
   proves the rest (its log shows no `exit status 2`).
3. **A deploy with `distinctInstance` and managed scaling.** Push a new tag and redeploy with
   `ImageUri=$REPO:<new tag>`. Watch `aws autoscaling describe-auto-scaling-groups
   --query 'AutoScalingGroups[?contains(AutoScalingGroupName,`roll-search`)].Instances'`: the
   group grows to twice the running nodes, new tasks start on the new instances, the old ones
   hand their clusters over and stop, and after about 15 minutes the empty instances are
   removed. The deploy must finish without a circuit-breaker rollback. Meanwhile run a loop of
   `curl` searches: none should fail.
4. **Scale to zero and the wake.** Redeploy with `MinNodes=0 IdleMinutesToZero=5`, send nothing
   for 5 minutes plus a few for the metrics, and watch `aws cloudwatch describe-alarms
   --alarm-name-prefix roll-search --query 'MetricAlarms[].[AlarmName,StateValue]'` (the idle
   alarm goes to ALARM, then back to OK once no node runs) and the node service's desired count
   go to 0. Then time one request: `time curl -u logs:<password> "$(out Endpoint)docs/_count"`
   (expect about a minute on EC2, 20–40 s on Fargate) and check the node is not stopped again
   in the next 5 minutes while you send a request every minute. Also confirm the alarm's `tasks`
   term: `aws cloudwatch get-metric-statistics --namespace AWS/ECS --metric-name CPUUtilization
   --statistics SampleCount --period 60 …` reports one sample per running task per minute.
5. **Spot interruptions.** With a Spot node running (`MinNodes` above `OnDemandNodes`), interrupt
   it with AWS Fault Injection Service (action `aws:ec2:send-spot-instance-interruptions`, target
   the instance, `durationBeforeInterruption` 2 minutes), while a writer runs against a cluster
   the node holds. The node's log must show `shutting down: handing clusters over` and the task
   must stop well inside the two minutes (the local rehearsal drained in about a second; real S3
   flushes take longer per cluster), with no acknowledged write missing afterwards.
6. **Upgrades of a running fleet.** Nothing earlier than this release ever ran on AWS, so the
   first deploy starts on the current fleet protocol. When a later release changes it (as
   `api_tls` did: nodes serving their control API over pinned TLS), update the proxies before
   the nodes: register the proxy task definition with the new image and `aws ecs update-service
   --cluster <Cluster> --service <proxy service> --task-definition <new revision>`, wait for it
   to settle, then deploy the stack with the new `ImageUri`. The mixed fleet that produces is
   tested locally (`a_fleet_from_before_api_tls_upgrades_proxies_first`): new nodes call old ones
   in plain HTTP, and while any node record lacks the API pin (an old node) a new node's TLS API
   port also answers plain HTTP, so console calls an old node forwards keep working; once the
   last old node stops it answers TLS only again.

### 7. Tear it down

`aws cloudformation delete-stack --stack-name roll-search` removes everything except what holds
data: the bucket (`Bucket` output; `DeletionPolicy: Retain`) and a final RDS snapshot. Delete
those, and the ECR repository, by hand when you are sure.

## Parameters

| parameter | default | |
| --- | --- | --- |
| `CertificateArn` / `Domain` | empty | Empty: a Lambda function URL in front (HTTPS on an AWS domain, about $0.40 per million requests with duration, 6 MB request and response limit). Set: an internet-facing NLB with TLS 1.3/1.2 on 443 (Elasticsearch REST) and 8443 (control API), about $20/month with no per-request charge and no payload limit; cheaper above roughly 50 million requests a month, and the only choice for bulks over 6 MB. |
| `ClusterHostNames` | `false` | With the NLB, give clusters their own host names (`<cluster>.<Domain>`, SNI/`Host` routing; needs a wildcard certificate). Otherwise clusters share the endpoint and are told apart by their credentials, which every client sends anyway. |
| `ClientCidr` | `0.0.0.0/0` | Who may reach the NLB. |
| `NodeCompute` | `ec2` | `ec2`: Graviton instances from an Auto Scaling group behind an ECS capacity provider (`OnDemandNodes` on demand, the rest Spot, `price-capacity-optimized` over every current Graviton type of the size). `fargate`: Fargate for `OnDemandNodes`, Fargate Spot above; no instances, faster cold starts, higher price. |
| `MinNodes` / `MaxNodes` | 1 / 8 | Nodes always running, and the most there may be (up to 100). `MinNodes=0` lets the fleet stop (see below). With `ec2` the Auto Scaling group may grow to 200 instances: a deploy starts each new node on an instance of its own before the old one stops (2 × the running nodes), and ECS managed scaling launches only what tasks need. |
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
an alarm sets the node service to 0 tasks (each node drains first). The proxies stay up. The
alarm's 0/1 expression breaches its 0.5 threshold by +0.5, inside the step's `[0, ∞)` (AWS
documents no step for a breach exactly at the threshold). It only counts minutes in which nodes
run, so it returns to OK once the fleet is at zero. An alarm in ALARM re-runs its scaling action
every minute, and through the drain and the metric lag after the stop it still is: the policy's
600 s cooldown keeps it from stopping the node a wake starts meanwhile
(`tests/search/aws/model.py` simulates this minute by minute, with metrics one to three minutes
late).

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

## What was verified locally

Everything below runs without AWS credentials; the commands are in step 1.

- **Template**: `cfn-lint` clean (both this and `deploy/queues/aws/roll-queues.yaml`).
- **Scaling rules** (`scripts/search-aws-rehearsal.sh model`, `tests/search/aws/model.py`, with
  AWS's documented step-bound and alarm semantics): every alarm breach lands strictly inside a
  step (`[{ MetricIntervalUpperBound: 0 }]`, the bug the queues template had, matches nothing);
  scale to zero within the idle period plus the metric lag; a wake at any minute around the stop
  answered within two minutes and never undone (only the to-zero cooldown prevents that, with
  metrics one to three minutes late); scale-in one node at a time, never the last; a deploy at
  full size fits the Auto Scaling group (it didn't: `MaxSize` was `MaxNodes`; the queues
  template's broker group had the same shape and now has the same headroom); the node task's
  cache and memory arithmetic run by dash for every `NodeCacheGiB`; stop and hold budgets
  (drain 100 s < StopTimeout 120 s ≤ Spot's two minutes; proxy hold 120 s < function timeout
  150 s); nodes and the database admit nothing from outside.
- **Node instance user data on Amazon Linux 2023** (`scripts/search-aws-rehearsal.sh userdata`):
  run in `amazonlinux:2023` against loop devices: the `/dev/xvdb` link (Nitro), no link with a
  blank instance-store disk listed first (never taken; the old user data took it), an already
  formatted volume kept, no volume at all (powers off, the ECS agent never configured); AL2023's
  own `ecs-init` package orders `ecs.service` after `cloud-final.service`, so the volume is
  mounted before any task can be placed.
- **The stack on LocalStack 4.9** (`scripts/search-aws-rehearsal.sh localstack`): every resource
  LocalStack implements deploys to `CREATE_COMPLETE` and is checked: VPC and three subnets in
  three AZs, security groups, the S3 gateway endpoint, the bucket (encryption, public access
  block, ownership), the three secrets, the IAM roles and their policies, the log groups, the
  Lambda function (image, `EntryPoint` + `Command [--lambda]`, arm64, timeout, the TLS seed
  resolved from its secret), its URL and permissions, the idle and scale-in alarms. LocalStack
  doesn't implement ECS, ELBv2, RDS, Auto Scaling, Application Auto Scaling, launch templates,
  security group ingress resources or S3 Express (the run lists them), applies no security group
  rules, bucket lifecycle rules or log retention, and fails on `!Select [i, !Cidr …]` (computed
  for it). Two template changes came out of it, both equivalent on AWS: the TLS seed's dynamic
  reference uses the whole-secret form, and `ScaleToZero` compares `MinNodes` with the string
  `"0"`.
- **The Lambda path end to end**: the deployed function's image, `EntryPoint`, `Command` and
  environment (read back from LocalStack, whose community edition doesn't run image functions)
  under AWS's Lambda Runtime Interface Emulator, arm64 under qemu, behind a function URL front
  (payload format 2.0): clusters created through the control API and the image smoke test's
  Elasticsearch requests through it, over TLS pinned to the certificate derived from the stack's
  seed to a real proxy and node; a proxy with another seed gets nothing.
- **The images** (`scripts/search-image-smoke.sh`; `scripts/test-search.sh --images`), linux/amd64
  and linux/arm64 (under qemu): two nodes started with the template's own node command and a
  proxy with the template's proxy command, metadata in Postgres 18; clusters created through the
  proxy's API port, index/bulk/get/search with aggregations/count/errors/`_cat` through its REST
  port; clusters sleeping and waking; a Spot-style `docker stop -t 120` of one node under four
  writers (drained in 0.5 s on amd64, 1.1 s on arm64, exit 0, every acknowledged write readable
  from the other node); scale to zero (the last node stopped) and the wake: a held request, the
  proxy fetching task-role credentials and calling `DescribeServices` then `UpdateService
  desiredCount 1` on a fake ECS that checks every SigV4 signature, a fresh node with an empty
  disk answering the held request with every acknowledged write. With `--s3` the same run keeps
  segments, WAL and manifests in an S3 bucket (LocalStack 4.9) through the image's own S3 client,
  with the task-role credentials fetched from the container credentials endpoint as on ECS
  (a relative URI at the agent's link-local address); node b instead gets a full URI
  (`AWS_CONTAINER_CREDENTIALS_FULL_URI`) with `AWS_CONTAINER_AUTHORIZATION_TOKEN`, as container
  runtimes other than ECS set it, and must fetch from it (pg-storage's own provider: `object_store`
  alone would have fallen back to the instance metadata service).
- **The fleet** (`crates/search-control/tests/fleet.rs`, on SQLite and on Postgres 18): one owner
  per cluster, failover after a kill, fencing, SIGTERM handoff under load, the wake from zero, a
  `_bulk` refused mid-upload and resent, pinned TLS on both ports, and an upgrade from a fleet
  before `api_tls` (node records without the column, an old node with a plain control API): the
  new proxy serves it, old and new nodes run side by side, the old node's SIGTERM hands its
  clusters to the new one without losing a write; while the old node runs, the new node's TLS API
  also answers its plain HTTP calls, and once it has left, only TLS. Always-on clusters
  (`idle_timeout_s` 0) load as soon as they are created and come back up on another node when
  theirs stops, with no request asking for them; nodes find them through a partial index, so the
  poll every three lease periods costs the same however many other clusters the catalog holds.
- **The product surface** against a persisted `rolld`: `roll search clusters
  create/list/show/set/branch --at/password/url/unload/indices/usage` and `roll search request`
  (bulk from stdin), data and usage surviving a restart; `/dashboard/search` (`pnpm --filter
  dashboard typecheck` and `build`) listing clusters, a cluster's overview and usage against it.
  Also with `rolld --search-only --s3-bucket` on LocalStack S3 and full-URI container
  credentials: a restart with an empty local cache reads every document back from the bucket.

The pieces the stack runs are also tested on their own:

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
- the image itself (`deploy/search/Dockerfile`): release builds for linux/arm64 (cross-compiled
  on x86, aarch64 binaries needing glibc 2.35 at most, about 95 MB compressed) and linux/amd64,
  both run by `scripts/search-image-smoke.sh` (above).

The fleet, fencing, durability and persistence tests also pass against an S3-compatible server
(SeaweedFS: `ROLL_E2E_S3_BUCKET`, `ROLL_E2E_S3_ENDPOINT`, `AWS_*` credentials) and against
Postgres 18 (`ROLL_SEARCH_TEST_PG`).
