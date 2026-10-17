# roll tasks on AWS

There are two layouts. Both use the same image (`deploy/aws/Dockerfile`), API, SDK and CLI.

| | `tasks.yaml` (default): log store | `tasks-dynamodb.yaml` |
| --- | --- | --- |
| State | roll's own log store: SQLite on the leader, group commits and snapshots in S3 | DynamoDB on-demand (+ S3 for large bodies) |
| Per million messages | $0.08–$5 of S3 depending on concurrency (about $0.26 on a shared cell), plus $0.40 of Lambda or $0.02 of load balancer | about $6.90 at any volume |
| Always on | 2 nodes, about $9.40/month (+ $16 with a load balancer) | 1 Spot worker, about $2.20/month |
| Tenants | many per cell, sharing its commits | one per stack |
| Writers | one leader per cell | any number of workers |
| Failover | standby takes over in seconds; Lambda holds requests meanwhile | none needed |

## The log store layout

```
                      ┌──────────────── VPC (no NAT) ─────────────────────────┐
            ┌──────────────┐ forwards  ┌────────────┐  health,    ┌─────────────┐ │
 clients ──▶│ Lambda proxy │──────────▶│ leader     │◀── forward ─│ standby     │ │
 SDK / CLI  │ or NLB (TLS) │           │ SQLite     │             │ (Spot)      │ │
            └──────┬───────┘           └─────┬──────┘             └──────┬──────┘ │
                   │ reads lease             │ commits, snapshots,       │ lease  │
                   └─────────────────────────┴─▶ S3 (gateway endpoint) ◀─┘        │
                      └───────────────────────────────────────────────────────┘
                                             └──▶ your endpoints (public IPs)
```

- **Nodes.** ECS on Fargate, arm64. `OnDemandNodes` of them run on regular Fargate and the rest on
  Spot, spread over two AZs.
  - Exactly one node leads. It serves the API, delivers messages and owns the data.
  - The others stand by. They check the leader's health over the private network (free) every
    second and forward any requests they get.
- **Data.** Everything lives in one S3 bucket under `tasks/`:
  - `wal/<seq>`: one object per group commit, holding the changes of every tenant written since
    the previous one
  - `snap/<tenant>/<seq>.db.zst`: compressed snapshots of each tenant; the last two are kept
  - `lease`: who leads and where
- **Tenants.** The cell hosts a `default` tenant (`/v1/tasks`) and any number of others
  (`/t/<tenant>/v1/tasks`). Each has its own database, engine and API keys; a hosted tenant's API
  takes only its own keys. The node's token manages them:

  ```sh
  roll tasks tenants create acme    # prints ROLL_TASKS_URL for acme and its first API key
  roll tasks tenants list
  roll tasks tenants key acme --name ci
  roll tasks tenants delete acme    # and all of its data
  ```

  That's how one cell serves many small customers for the price of one: their writes share
  commits, and an idle tenant costs nothing.
- **Writes.** API writes return once their commit is in S3. Worker bookkeeping is committed within
  5 seconds (`ROLL_TASKS_RELAXED_MS`). Recovered state is always an exact prefix of history: a crash
  loses at most those seconds of bookkeeping, which turn into redeliveries (at-least-once).
- **Failover.**
  - **Crash.** After three failed health checks (about 3s), a standby wins the lease with a
    compare-and-swap. It rebuilds from the last snapshot plus the log and serves. That takes the
    time to download and decompress the snapshot; snapshots keep the log to replay short.
  - **Deploy or Spot reclaim (SIGTERM).** The leader drains deliveries, commits, releases the lease
    and exits. A standby takes over in about a second.
  - **During a switch.** The Lambda proxy (or, behind the load balancer, the standby that got the
    request) holds requests for up to 25s. A request cut off by the switch gets a retryable 503,
    and the proxy retries it.
- **Fencing.** Log objects are written create-only (`If-None-Match: *`). A new leader writes a
  takeover entry before serving, so an old leader that's still running can't commit anything; it
  notices and exits. Correctness never depends on clocks or health-check timing.
- **Network.** The stack builds its own VPC with two public subnets, an S3 gateway endpoint (free)
  and no NAT gateway.
  - Nodes have public IPs for outbound webhooks; nothing inbound reaches them except the front door
    and each other on port 8080.
  - Lambda runs in the VPC to reach the leader's private IP. The load balancer sends to any healthy
    node, and standbys forward to the leader.

### Deploy

```sh
REGION=us-east-1
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
REPO=$ACCOUNT.dkr.ecr.$REGION.amazonaws.com/roll-tasks

aws ecr create-repository --repository-name roll-tasks --region $REGION
aws ecr get-login-password --region $REGION | docker login --username AWS --password-stdin ${REPO%/*}
docker buildx build --platform linux/arm64 --provenance=false -f deploy/aws/Dockerfile -t $REPO:v1 --push .

aws cloudformation deploy --region $REGION --stack-name roll-tasks \
  --template-file deploy/aws/tasks.yaml --capabilities CAPABILITY_IAM \
  --parameter-overrides ImageUri=$REPO:v1 Token=$(openssl rand -hex 32)

aws cloudformation describe-stacks --stack-name roll-tasks --query 'Stacks[0].Outputs'
export ROLL_TASKS_URL=<ApiUrl output> ROLL_TASKS_TOKEN=<the token>
roll tasks api-keys create ci     # scoped keys for apps; keep the admin token for yourself
```

With your own domain, put a load balancer in front instead of Lambda: request an ACM certificate
for it, add `CertificateArn=arn:aws:acm:… Domain=tasks.example.com` to the parameters, and point
the domain (CNAME) at the `LoadBalancer` output. It costs about $16/month with no per-request
charge, so it's cheaper above roughly 50 million calls a month.

To upgrade, push a new tag and redeploy with `ImageUri=$REPO:v2`. New nodes join as standbys
before old ones hand over, so the API stays up throughout. Lambda switches atomically.

### Backups and point-in-time restore

The bucket is versioned. Whatever the cell overwrites or deletes (pruned commits, replaced
snapshots, prune markers) stays as a noncurrent version for `BackupDays` (30 by default), then
expires. That protects against a bug, a bad deploy or a mistaken delete: you can put the whole
cell back to how it was at any second in that window.

```sh
STACK=roll-tasks
out() { aws cloudformation describe-stacks --stack-name $STACK --query "Stacks[0].Outputs[?OutputKey=='$1'].OutputValue" --output text; }
CLUSTER=$(out Cluster) SERVICE=$(out Service) LOG=s3://$(out Bucket)/tasks

# 1. Stop the cell. Leaders hand over and release the lease as they stop.
aws ecs update-service --cluster $CLUSTER --service $SERVICE --desired-count 0
aws ecs wait services-stable --cluster $CLUSTER --services $SERVICE

# 2. See what the restore would do, then do it.
cargo run -q -p tasks --release --bin roll-tasks -- --log $LOG restore --at 2026-10-17T09:30:00Z
cargo run -q -p tasks --release --bin roll-tasks -- --log $LOG restore --at 2026-10-17T09:30:00Z --apply

# 3. Start the cell again (Nodes, 2 by default).
aws ecs update-service --cluster $CLUSTER --service $SERVICE --desired-count 2
```

- **What comes back.** Every tenant, message, run and key as a crash at that second would have
  left them (everything acknowledged by then), including tenants deleted since. Anything created
  later is gone from the restored cell.
- **Nothing is destroyed.** The restore copies old versions back and puts delete markers on newer
  objects; the newer versions stay in the bucket. Restoring to a later time undoes it.
- **The cell must be stopped.** The restore refuses to run while a node holds the lease. After a
  crash that left it held, check that no node is running and pass `--force`.
- **It's downtime.** The API is unavailable from step 1 until a leader is back in step 3. Expect
  a minute or two for ECS, plus the restore, which takes a few seconds per thousand changed
  objects.
- **Your credentials** need `s3:GetBucketVersioning`, `s3:ListBucketVersions`, `s3:GetObject`,
  `s3:GetObjectVersion`, `s3:PutObject` and `s3:DeleteObject` on the bucket.
- **Cost.** Storage only, at $0.023/GB-month: about `BackupDays` worth of commits plus the
  snapshots replaced in that time. Expiring old versions is free.
- **Not covered.** Commits in an S3 Express bucket (`ExpressZoneId`): directory buckets have no
  versions. Also not covered: losing the whole bucket or region (S3 replication would be the next
  step).

### Sizing and limits

| knob | effect |
| --- | --- |
| `NodeCpu` / `NodeMemory` | The leader does all of the cell's work. Start at 0.25 vCPU / 0.5 GB and raise for throughput or many concurrent slow deliveries. |
| `Nodes` | 2 means one standby. More standbys don't add throughput. |
| `OnDemandNodes` | 1 (default) keeps a node even if Spot capacity disappears in both AZs at once. 0 saves about $5/month. |
| `NodeStorageGiB` | Local disk for the working database, which is what `Retention` keeps. |
| `MaxInflight` | Concurrent deliveries. |
| `Retention` | How long finished messages, runs and events are kept. Costs storage only. |
| `CertificateArn`, `Domain` | Serve the API from a Network Load Balancer instead of Lambda. |
| `BackupDays` | How far back a point-in-time restore can go (30). |

- **Throughput.** Throughput is that of one leader. On a laptop (4 runtime threads) it reached about
  1,300 published and delivered messages per second against a 30 ms S3. A 0.25 vCPU node will do
  much less. Measure with the `cost_profile` test, or load-test your stack, before relying on a
  number.
- **Scaling past one leader.** Beyond one leader, run more cells (one stack each) and place tenants
  on them, or use the DynamoDB layout. Cells don't yet move tenants between each other
  automatically.
- **Commit latency.** An API write waits for the next commit window (up to `ROLL_TASKS_COMMIT_MS`,
  100 ms) and one S3 PUT, typically 20–60 ms on S3 Standard. One client publishing one message at a
  time therefore gets about 10 a second; use `/batch` or concurrency for more.
- **S3 Express for the log (`ExpressZoneId`).** The template can put the commits alone in an S3
  Express One Zone bucket, with snapshots and the lease staying in S3 Standard
  (`ROLL_TASKS_WAL=s3express://<name>--<az-id>--x-s3/tasks`). The trade-offs:
  - Each commit costs $1.13 per million PUTs instead of $5.
  - Commits take single-digit ms instead of 20–60 ms.
  - The commits since the last snapshot live in that one AZ. If the AZ is lost, so are they;
    while it's down, a new leader can't start.

  Fencing relies on create-only writes (`If-None-Match`) in the directory bucket. The store
  handles its unsorted listings, and the split layout is tested locally. It hasn't been run
  against real S3 Express, so try it on a test stack first.

### Cost (us-east-1 list prices)

- **Fixed.**
  - Two 0.25 vCPU / 0.5 GB nodes: about $7.20 on demand plus $2.20 on Spot.
  - Lease checks: about $0.25 of S3 GETs.
  - Total: **≈ $9.40/month**.
  - An idle cell makes no S3 writes.
- **Storage.**
  - S3 bills each commit ($5 per million PUTs).
  - Commits group whatever every tenant wrote since the previous one, so the busier the cell, the
    cheaper each message. With a 30 ms simulated S3:

    | traffic | S3 PUTs per message | storage cost / million messages |
    | --- | --- | --- |
    | 1 publisher at a time | 1.0 | $5.00 |
    | 8 concurrent | 0.13 | $0.62 |
    | 64 concurrent | 0.016 | $0.08 |
    | 200 tenants, 1 message/s each | 0.05 | $0.26 |

  - A commit starts at most every `ROLL_TASKS_COMMIT_MS` (100 ms), so storage is capped at
    10 PUTs a second, about $130/month, however many messages flow. Lower it for latency, raise it
    to lower the cap.
  - Each tenant's usage carries its share of the commits it was in.
  - With `ExpressZoneId`, commits cost about a quarter as much: the shared-cell row drops to about
    $0.06.
- **Front door.**
  - Lambda proxy: about $0.40 per million API calls (256 MB, waiting for the commit).
  - Network Load Balancer: about $16/month plus about $0.006 per GB, which is roughly $0.02 per
    million calls.
- **A shared platform.** Many tenants in one cell behind the load balancer: about $0.30 per million
  messages before egress, plus about $26/month for the cell.
- **Egress.** $0.09/GB of responses.

`roll tasks usage` and the dashboard price your actual traffic from the S3 writes the store
metered (`crates/tasks/src/cost.rs`).

## The DynamoDB layout

```
            ┌──────────────┐      ┌──────────────────────┐
 clients ──▶│ Lambda (API) │─────▶│ DynamoDB (on-demand) │◀────┐
 SDK / CLI  │ function URL │      │ one table, TTL       │     │
            └──────────────┘      └──────────────────────┘     │
                   │                         ▲          ┌──────┴─────────┐
                   └──── S3 (bodies > 32 KB) ┴──────────│ Fargate Spot   │──▶ your endpoints
                                                        │ workers, arm64 │
                                                        └────────────────┘
```

- **API.** Lambda, scaled per request.
- **Workers.** ECS Fargate Spot, autoscaled on delivery-slot saturation (a CloudWatch EMF metric),
  splitting due-index lanes by rendezvous hashing.
  - A dead worker's lanes move to the others within about 20s.
  - SIGTERM drains the worker.
- **State.** One DynamoDB table (on-demand, PITR, deletion protection) with TTL, plus a bucket for
  large bodies. Both are retained if the stack is deleted.
- **Cost.** About 10 write units per message, which is $6.25 of DynamoDB writes plus reads, Lambda
  and egress: **≈ $6.90 per million messages**, plus about $2.20/month per Spot worker.

```sh
aws cloudformation deploy --region $REGION --stack-name roll-tasks \
  --template-file deploy/aws/tasks-dynamodb.yaml --capabilities CAPABILITY_IAM \
  --parameter-overrides ImageUri=$REPO:v1 Token=$(openssl rand -hex 32) \
    VpcId=vpc-… SubnetIds=subnet-…,subnet-…
```

| knob | effect |
| --- | --- |
| `MaxWorkers` | Upper bound for autoscaling. Useful workers ≤ lanes (`Shards` + queue partitions). |
| `Shards` | Each shared lane moves about 250 messages/s. Raise it for more throughput (it can't be lowered). |
| queue `partitions` | Spread one hot queue over several lanes: `roll tasks queues set q --partitions 8`. |
| `MinWorkers` ≥ 2 | No delivery pause while a reclaimed Spot task is replaced. |

## Notes

- The token is passed to Lambda and ECS as an environment variable, `NoEcho` in CloudFormation.
  Anyone who can read the function or task configuration can see it. Move it to Secrets Manager if
  that matters in your account.
- `ROLL_TASKS_BLOCK_PRIVATE` is on, so deliveries can't reach private, link-local or metadata
  addresses, even through DNS.
- Delays are capped at a year.
- Both layouts keep their bucket (and table) if the stack is deleted.
