# roll cloud on AWS and Cloudflare

How to stand up, deploy, scale and tear down roll cloud: every product on one fleet of `rolld`
nodes in us-east-1, behind Cloudflare. The design is [docs/cloud/README.md](../../docs/cloud/README.md).

| file | what |
| --- | --- |
| `Dockerfile` | the fleet image: `rolld` (release, every product) and the dashboard's Node server; target `fetch`, roll's small image beside customer code in isolated tasks (`roll-fetch`) |
| `entrypoint.sh`, `health.sh` | the image's entrypoint (node id, zone and address from instance metadata) and health check |
| `aws/roll-cloud.yaml` | one CloudFormation stack: VPC, DynamoDB, S3, KMS, secrets, ECR, ECS on Graviton, NLB, alarms, GitHub OIDC |
| `deploy.sh` | build and push, canary, bake, roll, rollback |
| `cloudflare/zones.json`, `cloudflare/apply.mjs` | DNS records, zone settings and Worker routes for roll.sh and roll.site |
| `../../.github/workflows/deploy.yml` | runs `deploy.sh` on every merge to `main` |

## How it fits together

- Each EC2 instance runs one ECS task with two containers from the same image: `rolld` and
  `dashboard` (the dashboard on 127.0.0.1:3000; rolld proxies to it). Two containers, not one
  entrypoint with a supervisor: ECS restarts, logs and limits each process on its own, and a
  dashboard crash never stops the node.
- The task uses host networking. The node owns the instance's ports, so two nodes never share an
  instance. That includes the canary.
- The NLB's targets are the instances. The instance group registers each one in all nine target
  groups. The health check (`GET :7070/healthz`) decides which instances get traffic.
- Secrets come from Secrets Manager, sealed with the stack's KMS key. Nodes reach S3 and DynamoDB
  through free gateway endpoints. There is no NAT gateway: instances have public addresses, and
  their security group only admits the NLB and the other nodes.
- Customer code (roll.toml checks, previews, agent sandboxes) never runs on an instance: each run
  is a Fargate task in the same cluster, in three isolated subnets whose network ACL and security
  group keep it away from the nodes (nodes reach a preview on `PreviewPort`, nothing else gets
  in), with a task role that has no permissions and a public IP as its only way out. The
  templates are `<stack>-check`, `<stack>-preview` and, with a `SandboxImage`, `<stack>-sandbox`;
  rolld registers a copy per customer image. Their logs are in the `RunnerLogGroup` output.
  docs/cloud/README.md, "Isolated work".

## Stand it up from zero

You need the AWS CLI, Node 24, pnpm, Docker with buildx, and the GitHub CLI.

### 1. Domains and Cloudflare

1. roll.sh is already a Cloudflare zone. Buy roll.site, add it to the same Cloudflare account,
   and set its nameservers to Cloudflare's. Wait until the zone is active.
2. Make two API tokens (My Profile → API Tokens → Create Token → Custom):
   - **apply**: Zone Read, DNS Edit, Zone Settings Edit and Workers Routes Edit, on roll.sh and
     roll.site. You use it on your machine.
   - **rolld**: DNS Edit on roll.sh and roll.site, nothing else. The nodes use it for ACME DNS-01.
     It goes into Secrets Manager in step 3.

### 2. GitHub

1. Create the environment `production` (Settings → Environments). Under "Deployment branches and
   tags", allow only `main`.
2. Put the ref into the OIDC subject claim. The deploy role only trusts
   `repo:roll-sh/roll:environment:production:ref:refs/heads/main`:

   ```sh
   gh api -X PUT repos/roll-sh/roll/actions/oidc/customization/sub \
     -f 'include_claim_keys[]=repo' -f 'include_claim_keys[]=context' -f 'include_claim_keys[]=ref' \
     -F use_default=false
   ```

   The repository uses GitHub's immutable subjects, so the claim names it by id:
   `repo:roll-sh@343565064/roll@1422227286:environment:production:ref:refs/heads/main` (the stack's
   `GitHubRepo` default). `gh api repos/roll-sh/roll/actions/oidc/customization/sub` shows the
   prefix (`sub_claim_prefix`).

### 3. Create the stack

```sh
aws s3 mb s3://roll-cloud-templates-<account> --region us-east-1   # once: the template is over 51,200 bytes
aws cloudformation deploy --region us-east-1 --stack-name roll-cloud \
  --template-file deploy/cloud/aws/roll-cloud.yaml --capabilities CAPABILITY_NAMED_IAM \
  --s3-bucket roll-cloud-templates-<account> --s3-prefix roll-cloud
```

Every later `aws cloudformation deploy` of the stack takes the same `--s3-bucket` and
`--s3-prefix` (CloudFormation reads templates over 51,200 bytes from S3 only).

- The first create runs no nodes (`ImageTag` is `none`: the image repository is still empty).
  It makes the instances, the NLB and everything else.
- An AWS account holds one GitHub OIDC provider. If yours has one, add
  `--parameter-overrides GitHubOidcProviderArn=arn:aws:iam::<account>:oidc-provider/token.actions.githubusercontent.com`.
- Parameters are listed at the top of the template: instance types, Spot mix, node counts, alarm
  thresholds, Elastic IPs, and isolated work's `PreviewPort`, `ChecksOnSpot` and `SandboxImage`
  (the agent sandbox image, `deploy/agent/Dockerfile` built for arm64 and pushed where Fargate can
  pull it; empty runs no sandboxes). The defaults are the testing size in docs/cloud/README.md,
  "Cost".

Read the outputs:

```sh
aws cloudformation describe-stacks --region us-east-1 --stack-name roll-cloud --query 'Stacks[0].Outputs' --output table
```

Fill in the two secrets the stack can't generate. `deploy.sh` refuses to deploy until both are set.

```sh
aws secretsmanager put-secret-value --region us-east-1 --secret-id roll-cloud/cloudflare-api-token --secret-string '<the rolld token>'
aws secretsmanager put-secret-value --region us-east-1 --secret-id roll-cloud/system-database --secret-string 'postgres://…?sslmode=verify-full'
```

Until the system database runs on the fleet, `ROLL_SYSTEM_DATABASE` can be any Postgres 18 (RDS,
or another provider). The image trusts Amazon RDS's CA bundle.

The other secrets (cluster secret, `ROLL_SECRET_KEY`, `ROLL_ADMIN_TOKEN`, and
`ROLL_QUEUES_PASSWORD` for roll queues' `roll-admin` login) are generated. Read the admin token
with `aws secretsmanager get-secret-value --secret-id <arn>`. Keep `ROLL_SECRET_KEY`
safe: the stack keeps it when deleted, because the data in the bucket needs it.

In GitHub, set the `production` environment's variable `AWS_DEPLOY_ROLE_ARN` to the
`DeployRoleArn` output.

### 4. Cloudflare

1. Deploy the CDN Worker. Its secrets are listed in `apps/cdn-worker/wrangler.toml`.

   ```sh
   cd apps/cdn-worker
   pnpm exec wrangler secret put ROLL_CDN_ORIGIN_SECRET --env production   # and the others
   pnpm exec wrangler deploy --env production
   ```

2. Plan the zones. This reads Cloudflare and the stack's outputs and changes nothing:

   ```sh
   export CLOUDFLARE_API_TOKEN=<the apply token>
   node deploy/cloud/cloudflare/apply.mjs --dry-run --stack roll-cloud
   ```

   - `+`, `~` and `-` lines are what it would create, change and delete. It only changes or
     deletes records that carry its comment (`managed by roll: deploy/cloud/cloudflare (us-east-1)`).
   - `!` lines are conflicts: a record it didn't make is in the way (for example the current
     site at `roll.sh`). It never overwrites one. When you're ready to move that name to the
     fleet, delete the old record in Cloudflare, or add the comment to it to hand it over.
   - Zone settings (SSL Full (strict), Always Use HTTPS, TLS 1.2 at least, HSTS on roll.sh,
     authenticated origin pulls) apply to the whole zone. Check that anything else served from
     roll.sh has a valid certificate at its origin before you apply them.

3. Apply. It exits 1 if conflicts remain; run it again after you resolve them.

   ```sh
   node deploy/cloud/cloudflare/apply.mjs --stack roll-cloud
   ```

With Elastic IPs (the default), `origin.us-east-1.roll.sh` is three A records. Without them it's
a CNAME to the NLB. Every other name is a CNAME to it.

The region is one value: `region` in `zones.json` (the names that customers connect to directly,
such as `*.pg.us-east-1.roll.sh`), and the stack's own AWS region (`ROLL_EDGE_REGION` is
`AWS::Region`, so the hostnames always match the region that S3 clients sign with). For a second
region, create the stack there. Then copy `zones.json` with that `region`, keep only the records
whose names contain `{region}`, and drop `workerRoutes`. Apply the copy with `--file <copy>
--stack roll-cloud --region <region>`. Each file's comment names its region, so neither file
touches the other's records. The global names (`roll.sh`, `api.roll.sh`, `git.roll.sh`,
`*.roll.site`) stay in the first file.

### 5. The first deploy

1. Push the first image. Either run the `deploy` workflow (Actions → deploy → Run workflow on
   `main`): it builds and pushes, then stops at "bootstrap the stack first". Or push from your
   machine (an arm64 machine builds much faster than an emulated build):

   ```sh
   deploy/cloud/deploy.sh push          # tags the image with the current commit
   ```

2. Start the nodes on that image. CloudFormation waits until every node is healthy.

   ```sh
   aws cloudformation deploy --region us-east-1 --stack-name roll-cloud \
     --template-file deploy/cloud/aws/roll-cloud.yaml --capabilities CAPABILITY_NAMED_IAM \
     --s3-bucket roll-cloud-templates-<account> --s3-prefix roll-cloud \
     --parameter-overrides ImageTag=$(git rev-parse HEAD)
   ```

3. Check it: `curl -sI https://roll.sh/`, `curl -s https://api.roll.sh/…`, and
   `deploy/cloud/deploy.sh status`.

From now on every merge to `main` deploys.

## Deploys

Every merge to `main` runs `.github/workflows/deploy.yml`. It runs one deploy at a time; a newer
merge waits. The job:

1. Builds the image for linux/arm64 on an arm64 runner and pushes it to ECR as `<commit sha>`,
   and roll's `fetch` image as `fetch-<commit sha>`. Layers are cached in the `roll-cloud-cache`
   repository.
2. **Canary.** Registers a new task definition revision with the new image and scales
   `rolld-canary` to 1. The capacity provider adds an instance for it. One node of the new version
   joins the fleet and takes its share.
3. **Bake.** Waits 10 minutes. The deploy fails if any of the stack's deploy alarms goes to ALARM
   (`DeployAlarms` output) or the canary stops.
4. **Roll.** Updates `rolld` to the new revision. MinimumHealthyPercent is 100 and
   MaximumPercent allows exactly one extra node (`deploy.sh` computes it for the current count). So
   ECS starts one new node, waits until it's healthy, stops one old node, and repeats. The extra
   node needs an instance of its own; the capacity provider adds one and removes it afterwards.
   The ECS circuit breaker and the alarms roll this step back by themselves.
5. Scales the canary to 0.

Any failure, or a cancelled job, puts the previous revision back on `rolld` and scales the canary
to 0.

By hand, with credentials for the stack:

```sh
deploy/cloud/deploy.sh push <sha>       # build and push (skipped if the tag exists)
deploy/cloud/deploy.sh deploy <sha>     # canary, bake, roll
deploy/cloud/deploy.sh rollback         # put back the revision the last deploy replaced
deploy/cloud/deploy.sh current          # the tag the fleet runs
deploy/cloud/deploy.sh status
```

`BAKE_SECONDS` changes the bake. To go back to an older build through the same canary, run the
workflow by hand with that commit's tag.

### Stack updates

The pipeline registers task definition revisions outside CloudFormation. When you update the
stack, pass the tag that is running, or CloudFormation deploys the tag it last knew:

```sh
aws cloudformation deploy --region us-east-1 --stack-name roll-cloud \
  --template-file deploy/cloud/aws/roll-cloud.yaml --capabilities CAPABILITY_NAMED_IAM \
  --s3-bucket roll-cloud-templates-<account> --s3-prefix roll-cloud \
  --parameter-overrides ImageTag=$(deploy/cloud/deploy.sh current) [Name=Value …]
```

Parameters you don't name keep their values. A change to the task definition (environment,
sizes) rolls the fleet one node at a time, like a deploy, but without a canary. The isolated-work
templates take their `fetch` image from `ImageTag` too, so a stack update moves them to the
running tag's `fetch-<tag>`; deploys don't touch them.

## Scaling

- Nodes: service auto scaling keeps between `Nodes` (the floor, one per zone) and `MaxNodes`. It
  scales out on CPU (60%), memory (75%) and NLB flows per node (`ConnectionsPerNode`). Scale-in
  waits 10 minutes.
- Instances follow the nodes: the ECS capacity provider adds and removes them. Keep
  `MaxInstances` at least `MaxNodes + 2`: a deploy needs a spare instance, and so does the canary.
- Every instance is on-demand by default: `OnDemandInstances` (3, one per zone) and
  `OnDemandAbovePercent` (100) for deploy spares, the canary and scale-out. Spot can run out in a
  zone: on the first deploy it left one zone empty and a deploy's spare unlaunchable. Lower
  either to save money once losing a zone's capacity for a while is acceptable.
- `InstanceType`, `InstanceType2` and `InstanceType3` (t4g.medium, c6g.large, c7g.large: same
  size) are tried in order, so one type running out in a zone doesn't stop a launch.
- A stopping node drains: it fails its health check, the NLB takes it out within about 15 s, it
  hands its leases over and exits, within 120 s. Spot's two-minute notice, scale-in and instance
  refreshes all drain the node first (ECS managed draining).
- New ECS-optimized AMIs: update the stack (the launch template reads the latest AMI from SSM),
  then replace instances gradually:

  ```sh
  aws autoscaling start-instance-refresh --region us-east-1 --auto-scaling-group-name <group> \
    --preferences '{"MinHealthyPercentage": 100, "MaxHealthyPercentage": 200}'
  ```

## Alarms

| alarm | fires when | rolls a deploy back |
| --- | --- | --- |
| `roll-cloud-healthy-nodes` | fewer healthy nodes behind the NLB than `Nodes`, for 3 minutes | fails the bake |
| `roll-cloud-tcp-resets` | more than `ResetsPerMinute` resets from nodes and the NLB, in 3 of 5 minutes | yes |
| `roll-cloud-task-failures` | a node crashed, failed to start or failed its health checks, in the last 5 minutes | yes |
| `roll-cloud-errors` | more than `ErrorsPerMinute` HTTP 5xx answers, in 3 of 5 minutes | yes |
| `roll-cloud-latency` | the slowest p99 time to the response head of code, postgres, redis, storage, nosql or tasks above `LatencyP99Ms`, in 3 of 5 minutes | yes |
| `roll-cloud-handoff-failures` | a lease handoff failed (timed out with requests in flight, its release not written, or left by a drain), in the last 5 minutes | yes |
| `roll-cloud-lease-takeovers` | a node took over leases their holder never released (it died or lost DynamoDB), in the last 5 minutes | yes |

The bake watches every alarm in the `DeployAlarms` output; ECS rolls a deploy back on all but
`healthy-nodes`. The request and lease metrics come from rolld's JSON logs as Embedded Metric
Format records (docs/cloud/README.md, "Logs" and "Metrics"), and the error-log count from a
metric filter. Logs are in the `/roll-cloud/nodes` log group for 30 days. A shell on a node: `aws ssm start-session
--target <instance id>`; there is no SSH.

## Check the files locally

```sh
cfn-lint deploy/cloud/aws/roll-cloud.yaml
node --test deploy/cloud/cloudflare/test.mjs     # apply.mjs against a fake Cloudflare API
shellcheck deploy/cloud/*.sh
docker run --rm -v "$PWD:/repo" -w /repo rhysd/actionlint .github/workflows/deploy.yml
docker build -f deploy/cloud/Dockerfile --build-arg PROFILE=dev -t roll-cloud-test .
```

## Tear down

1. Remove the Cloudflare records the script made. Run it with a zones file that lists no records:

   ```sh
   echo '{"comment": "managed by roll: deploy/cloud/cloudflare (us-east-1)", "zones": {"roll.sh": {}, "roll.site": {}}}' >/tmp/empty.json
   node deploy/cloud/cloudflare/apply.mjs --file /tmp/empty.json
   ```

   Worker routes and zone settings stay. Remove the Worker with `pnpm exec wrangler delete --env production`.
2. Delete the stack: `aws cloudformation delete-stack --region us-east-1 --stack-name roll-cloud`.
   The image repositories are emptied and deleted with it. Instances are protected from scale-in
   while they run a node; if the delete waits on the instance group, remove the protection:
   `aws autoscaling set-instance-protection --no-protected-from-scale-in --auto-scaling-group-name <group> --instance-ids …`.
3. The stack keeps what holds data: the data bucket (`roll-cloud-data-<account>`), the KMS key and
   the `ROLL_SECRET_KEY` secret. To delete the data for good, empty the bucket (every version),
   delete it, delete the secret, and schedule the key's deletion. That can't be undone.
