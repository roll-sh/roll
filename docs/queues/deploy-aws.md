# Deploy roll queues on AWS

`deploy/queues/aws/roll-queues.yaml` is a CloudFormation stack for a production cluster:

```
 clients ──► NLB (cross-zone off: flows stay in their AZ)
               │
               ├─ mq-proxy (Fargate arm64, ¼ vCPU) ×3, one per AZ ─┐ picks a broker in its own AZ,
               │                                                   │ wakes brokers when none run
               ▼                                                   │
 brokers: rolld --queues-only (Graviton EC2: on-demand base + Spot) ×N ◄─┘
   │ metadata, ownership, change feed            │ logs (2-of-3 quorum appends)
   ▼                                             ▼
 Postgres (roll postgres or RDS)        roll-keeper ×3 (m7g, gp3), one per AZ ──► S3 (sealed segments)
```

- **Brokers** are stateless ECS tasks on Graviton instances: `OnDemandBrokers` on demand, the
  rest Spot (or Fargate, with `BrokerCompute=fargate`). Each one accepts every client and owns
  a share of the queues; ownership moves between brokers without clients reconnecting. The
  fleet is shared by every tenant and stays warm: tenants scale to zero, brokers don't.
- **Keepers** hold the hot tail of every log on gp3 and replicate each append to three AZs
  (two must fsync before a publisher confirm). Sealed segments move to S3 Standard.
- **Proxies** are the only thing the NLB talks to. They know which brokers are alive from the
  metadata store, send each connection to a broker in their own AZ, and start brokers when
  none are running.
- **Metadata** (vhosts, users, exchanges, queues, bindings, policies, queue ownership) lives in
  Postgres: a roll postgres database or any Postgres 14+.

## Deploy

1. Build the image for arm64 and push it to ECR:

   ```sh
   aws ecr create-repository --repository-name roll-queues
   docker buildx build --platform linux/arm64 -f deploy/queues/Dockerfile \
     -t <account>.dkr.ecr.<region>.amazonaws.com/roll-queues:latest --push .
   ```

   The image contains `rolld`, `mq-proxy` and `roll-keeper`.

2. Create two secrets: the metadata database URL and the broker admin password.

   ```sh
   aws secretsmanager create-secret --name roll-queues/meta \
     --secret-string 'postgres://roll:<password>@<host>:5432/roll_queues?sslmode=require'
   aws secretsmanager create-secret --name roll-queues/admin --secret-string '<admin password>'
   ```

   `sslmode=require` encrypts metadata traffic and verifies the server. The image trusts the
   RDS CA bundle as well as the system roots, so RDS works without extra setup.

   The stack generates a third secret itself, for internal traffic. Brokers prove it to the
   keepers and to each other before any request, so nothing else in the VPC can write logs or
   join the cluster.

3. Deploy the stack into a VPC with three private subnets in three AZs:

   ```sh
   aws cloudformation deploy --stack-name roll-queues \
     --template-file deploy/queues/aws/roll-queues.yaml --capabilities CAPABILITY_IAM \
     --parameter-overrides \
       ImageUri=<account>.dkr.ecr.<region>.amazonaws.com/roll-queues:latest \
       VpcId=vpc-… PrivateSubnets=subnet-a,subnet-b,subnet-c LoadBalancerSubnets=subnet-a,subnet-b,subnet-c \
       MetadataUrlSecretArn=arn:aws:secretsmanager:…:roll-queues/meta \
       AdminPasswordSecretArn=arn:aws:secretsmanager:…:roll-queues/admin
   ```

4. Connect with the `AmqpUrl` output (user `admin`). The NLB also serves Streams (5552), MQTT
   (1883), STOMP (61613), the HTTP API (8080) and the management API (15672). Add an S3 gateway
   endpoint to the VPC so tiering traffic is free.

| parameter | default | |
| --- | --- | --- |
| `StorageMode` | `keepers` | `keepers`: three AZs, survives an AZ outage. `s3express`: one S3 Express One Zone directory bucket (set `ExpressAzId`), no keepers; cheapest, but that AZ is a single point of failure. |
| `BrokerCount` / `MaxBrokers` | 2 / 12 | Warm brokers, and the most there may be. Brokers scale out on CPU (target 60%) and back in one at a time, never below `BrokerCount`. |
| `BrokerCompute` | `ec2` | `ec2`: Graviton instances from an Auto Scaling group behind an ECS capacity provider. `fargate`: no instances, Fargate prices. |
| `OnDemandBrokers` | 1 | With `ec2`, instances that are always on demand; the rest are Spot (`price-capacity-optimized`). |
| `BrokerVcpus` / `BrokerInstanceMemoryMiB` | 2 / 4096 | With `ec2`, the instance size. Any current Graviton type of that size qualifies (c7g, c8g, m7g, …), which widens the Spot pools. |
| `BrokerCpu` / `BrokerMemory` | 1024 / 2048 | Per broker task. |
| `IdleVhostSeconds` | 300 | A vhost without clients for this long is unloaded; it wakes on its next connection. |
| `VhostPublishRate` / `VhostPublishBytesRate` | 0 / 0 | Default publish budget of each vhost (messages/s, bytes/s; 0 = unlimited). |
| `IdleMinutesToZero` | 0 | With no client connection on the whole fleet for this long, brokers stop; `0` keeps `BrokerCount` warm. |
| `KeeperInstanceType` | `m7g.medium` | Keepers are disk- and network-bound. |
| `KeeperVolumeGiB` / `KeeperVolumeThroughput` | 100 / 250 | gp3; size for the hot tail, since older data is on S3. |
| `Scheme` / `ClientCidr` | `internal` / `10.0.0.0/8` | Who can reach the NLB. |

## A shared fleet

One fleet serves every tenant (vhost). What sleeps is the tenant, not the broker:

- **Idle vhosts unload** after `IdleVhostSeconds` without clients and timers: their queues
  flush and stop, and nothing of theirs is in memory. Their data stays in the logs.
- **Waking takes milliseconds.** The next connection, HTTP request or management call loads the
  vhost on the broker it reached. The broker reads only that vhost's metadata (an indexed
  lookup, however many tenants share the database) and opens its queues' logs in parallel, so
  a wake costs about one log open (a keeper round trip, or an S3 Express read), not one per
  queue. Vhosts wake independently: a large tenant waking doesn't hold up a small one.
- **Tenants can't crowd each other out.** Every vhost has a publish budget: `VhostPublishRate`
  messages/s and `VhostPublishBytesRate` bytes/s by default, or its own `max-publish-rate` /
  `max-publish-bytes-rate` limits (`rabbitmqctl set_vhost_limits`, or
  `PUT /api/vhost-limits/{vhost}/max-publish-rate` with `{"value": 5000}`; `-1` lifts the
  default). A vhost over budget is slowed down, not refused: its publishers wait, which holds up
  their channels and, through TCP, the clients, as RabbitMQ's flow control does. The vhost's
  `max-connections` and `max-queues` limits work as in RabbitMQ.

Stopping the brokers too (`IdleMinutesToZero`) only pays off for a deployment with a single
tenant that is idle for long stretches and can wait out a cold start.

## Spot

With `BrokerCompute=ec2`, everything above `OnDemandBrokers` runs on Spot, at roughly a third of
the on-demand price. Brokers hold no state, so an interruption costs a drain, not data:

1. AWS gives two minutes' notice. The ECS agent (`ECS_ENABLE_SPOT_INSTANCE_DRAINING`) marks the
   instance draining, and ECS starts replacement tasks elsewhere.
2. The broker gets SIGTERM and drains as in a rolling upgrade (below): its queues move to other
   brokers within the 90 s drain timeout, then its clients reconnect through the proxy.
3. Capacity rebalancing replaces instances that AWS flags as at risk before the notice comes,
   and the capacity provider keeps instances running brokers protected from scale-in until
   their tasks have moved.

Keep `OnDemandBrokers` at 1 or more, so a region-wide Spot shortage leaves the fleet smaller
but up. Keepers and proxies are never on Spot.

## Operations

- **Rolling upgrades.** Update the image and ECS replaces brokers one at a time (minimum
  healthy 100%). A stopping broker gets SIGTERM and then:
  1. hands every queue it owns to another broker (in-flight acks land first);
  2. closes its client connections gradually, so clients reconnect through the proxy to a
     broker that stays up.

  The task's stop timeout is 120 s and the drain timeout is 90 s.
- **Broker loss.** A broker that disappears stops heartbeating. After about 5 s, the others
  take its queues over: they fence its log epoch and replay from the keepers. Confirmed
  messages are on two keepers already, so nothing confirmed is lost.
- **Keeper loss.** Appends continue on the other two; a returning keeper is repaired
  automatically. Restart keepers one at a time and let each catch up before the next. Their gp3
  volumes are snapshotted when the stack deletes them.
- **Scale the whole fleet to zero** (only with `IdleMinutesToZero` set). When the NLB has had no
  active flow for that many minutes, an alarm sets the broker service to 0 tasks. The proxies
  and keepers stay up.
  - The next client connection waits at the proxy, which scales the service back to 1 through
    the ECS API.
  - The proxy forwards the connection as soon as the broker heartbeats. That is typically
    20–40 s on Fargate and about a minute on EC2 (an instance boots first), so give clients a
    connect timeout above that.
  - NLB health checks go to the proxy's separate health port and never wake brokers.
- **Streams through the NLB.** Stream clients ask for a stream's leader and then connect to
  it. Behind the proxy, use your client's load-balancer mode (for example
  `load_balancer_mode(true)` in the Rust client, or the address resolver in Java). The client
  then reconnects through the NLB until it reaches the broker that owns the stream.

## What it costs to run

The fixed part of the `keepers` mode is about three small keeper instances with their gp3
volumes, three ¼-vCPU proxy tasks, one NLB and `BrokerCount` warm brokers (one on demand, the
rest Spot), shared by every tenant. Brokers are added per instance as load grows. With
`s3express` there are no keepers: you pay for the proxies, the NLB and S3 Express requests and
storage. Appends of all of a broker's queues share S3 Express objects, so requests grow with
brokers and traffic, not with the number of queues.

Cross-AZ transfer is the largest per-message cost, and the stack keeps it small:
- clients reach a broker in their own AZ (the NLB and proxies are AZ-local);
- each queue's owner moves to the AZ most of its traffic comes from;
- replication to keepers is zstd-compressed (`ROLL_QUEUES_KEEPER_COMPRESS`, on by default).

How usage is priced from these costs, and how that compares with Amazon MQ and CloudAMQP for
the same workload, is on the dashboard's Usage page (`/dashboard/queues/usage`) and in
`roll queues usage`. Check current AWS prices for your region before sizing.

## Status

The template passes `cfn-lint`. It has not been deployed by our CI.

The pieces it runs are tested locally and in Docker:
- broker clusters, with failover, rebalance and a rolling restart under load
  (`crates/mq-cluster/tests`, `crates/rolld/tests/queues_cluster_e2e.rs`);
- keepers and S3 tiering (`crates/mq-storage/tests`, with MinIO);
- the proxy, with AZ preference and wake-on-connect (`crates/mq-proxy/tests`);
- queues following their clients to their AZ (`crates/mq-cluster/tests`), compressed
  replication (`crates/mq-storage/tests/quorum.rs`) and grouped S3 Express writes
  (`crates/mq-storage/tests/express.rs`, on an in-memory store);
- vhost wake latency and publish budgets (`crates/mq-core`);
- SigV4 signing of the ECS call, checked against AWS's documented example.
