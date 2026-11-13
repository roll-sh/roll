# roll apps: run your own code

roll apps hosts the user's own code next to their roll databases: a **service** in a workspace
project, built from a roll code repo (deploys on push), a GitHub repo mirrored into roll code, a
Dockerfile, a public image, or an auto-detected build. HTTP services **scale to zero, wake on the
first request, and cost nothing while idle**. Workers stay up, cron jobs run on a schedule, and
every deployment is immutable with instant rollback.

- Code: `crates/apps-build` (source detection and build plans; no I/O but reading the tree),
  `crates/apps-control` (the store, deploy queue, runtimes, the waking proxy, logs, metrics,
  metering, `/v1/apps`), mounted by `crates/rolld/src/apps.rs`.
- Contract: [http-api.md](http-api.md), with `apps/dashboard/src/lib/apps-api.ts`. Change both
  together.
- CLI: `roll up`, `roll deploy`, `roll logs`, `roll domains`, `roll apps …` (`crates/roll-cli/src/apps.rs`).
- Dashboard: services on the project canvas (`/dashboard/projects/$id`) and a service page at
  `/dashboard/apps/$id` (deployments, logs, metrics, variables, jobs, shell, domains, usage,
  settings).

```
  browser / curl ──▶ <slug>.up.roll.site, custom domains  (Cloudflare, proxied)
                          │
                    rolld (any node) ─ edge: Host → service ─┐
                          │                                   │ dispatch to the owner of
                          ▼                                   │ apps:service:<id>
               the service's owner node ◀────────────────────┘
               ├─ the waking proxy: queue the request, wake an instance, forward
               ├─ the deploy queue: build → image → roll out → health check → switch
               ├─ the autoscaler: concurrency per instance → instance count (min..max)
               └─ the meter: CPU-seconds, GB-seconds, requests, egress, cold starts
                          │
               Runtime: Docker on this host (one node, development)
                        Fargate tasks, CodeBuild builds (ROLL_APPS_RUNTIME=fargate; no customer
                        code on a node)
```

## What we learned from the others

| platform | what it gets right | what we take |
| --- | --- | --- |
| Railway | the project canvas; services, variables and private networking per project; environments (production, staging, PR envs) that copy a project; Railpack auto-detection; a deploy per push, rollback to any deployment; usage billing per second of real CPU and memory | the whole model: a service is a card on the project canvas, deploys per environment, variables per service and environment, `${{ postgres.DATABASE_URL }}`-style links, `railway up` as `roll up` |
| Render | blueprints, preview environments per PR, health-checked zero-downtime deploys, cron jobs as a service type | service types `web`, `worker`, `cron`; a health check gates every switch |
| Fly.io | Firecracker machines that **suspend** (memory snapshot) and resume in a few hundred ms; the proxy starts machines on request (`auto_start_machines`) and stops them when idle | the waking proxy and the tiered idle states: running → suspended → stopped |
| Cloud Run | request-based autoscaling on concurrency per instance, scale to zero, per-100 ms billing, startup CPU boost, min instances to avoid cold starts | concurrency target as the scaling signal; min/max instances; billing only while serving |
| Knative | the activator: requests for a scaled-to-zero revision are held in a buffer while the autoscaler brings a pod up; revisions are immutable | the owner's proxy is the activator; deployments are immutable revisions |
| Vercel | immutable deployments with their own URLs, instant rollback by moving the alias | every deployment keeps its image; rollback moves the environment's pointer, nothing rebuilds |
| Koyeb, Northflank | scale-to-zero on microVMs, BYO cloud, build + deploy pipelines with logs streamed live | build logs streamed as they happen; a deploy queue per service and environment |

## Decisions

### A service, an environment, a deployment

- **Service** (`svc_…`): what to run and how. It belongs to one workspace project and is a
  resource kind on its canvas (`service`, docs/workspace.md). Source, build settings, run
  settings (type, port, start command, scaling, resources, health check, volume) live on the
  service and apply to every environment.
- **Environment**: a name (`production`, `staging`, `development`, a custom one, a preview's
  `pr-42`). A service runs in an environment once it has been deployed there. Per environment:
  the branch that deploys on push, the variables, the active deployment, the domains, and the
  instances. This is Railway's model and roll secrets' (feature/roll-secrets): a service deploys
  into one of the project's environments and that environment's secrets are injected.
- **Deployment** (`dep_…`): an immutable record of one rollout: the source revision, the image
  it built (by digest), and a frozen copy of the service's settings and the variable names. A
  rollback deploys an older deployment's image and settings again, without building: it is a
  new deployment whose `rollback_of` names the old one, so history only grows.

### Builds

- **Auto-detect** (`crates/apps-build`): Railpack/Nixpacks-style providers read the tree and
  produce a **plan**: a base image, install, build and start commands, the port, and the
  Dockerfile they make. Providers, in order: Dockerfile (when one exists, it wins), Node (npm,
  pnpm, yarn, bun; Next, Remix, Astro, Vite, Nuxt, SvelteKit), Python (uv, poetry, pip; Django,
  FastAPI, Flask), Go, Rust, Ruby (Rails, Rack), PHP (Laravel, plain), Java (Maven, Gradle;
  Spring Boot), static sites (an `index.html`, or a framework build's output dir served by a
  tiny static server). The generated Dockerfile is shown in the UI and in the build log, so a
  build is never magic, and the service settings or `roll.toml` `[app]` can override any
  command.
- **Config as code** (`roll.toml` `[app]` in the service's root, like `railway.json`): `builder`,
  `dockerfile`, `install`, `build`, `start`, `pre_deploy`, `health_path`, `watch`. Read at each build;
  what it sets wins over the service's settings and is named in the build log and the plan's
  notes (`POST /v1/apps/plan` shows it too). It's checked as the API checks settings, an unknown
  key is an error (a typo never silently does nothing), and it must be a regular file. The
  file's other sections are roll code's; code-preview allows `[app]` in it. `[app.run]` sets any
  run setting (`type`, `port`, `min_instances`, `concurrency`, `cpu`, `memory_mb`, `schedule`,
  timings) but the volume, and `[app.environments.<name>]` (with its own `.run`) goes on top in
  that environment only, as Railway's environment overrides do. Without a file, an environment
  sets its own size and scale (`PUT …/environments/{env}` `run`: instances, CPU, memory,
  concurrency, suspend and sleep; `roll apps env --run min_instances=1`; "Its own size" in the
  dashboard) over the service's, checked with them as one run. The file still goes on top of
  both, as config as code wins on Railway. Changing one marks the environment `needs_deploy`,
  and an environment's own `min_instances` keeps the service owned, as the service's does. `[app.previews]` (and `.run`)
  goes on top in preview environments (ephemeral ones, `pv-<id>`), before a named block. A
  preview scales to zero unless that block sets `min_instances`, whatever production keeps up,
  so an open pull request costs nothing until someone opens it (Render's `previews.plan` and
  `numInstances`); an image service's previews do the same. A redeploy or a promotion
  applies the file at the deployment's revision for the target environment, so neither drops
  what the file set. What a push can't touch stays the service's: its volume, domains, access
  and variables.
- **Why a generated Dockerfile instead of Nix or buildpacks' lifecycle**: BuildKit's layer cache
  makes dependency installs incremental, multi-stage builds give slim runtime images (cold
  starts scale with image size), and one build path (Dockerfile → BuildKit) serves detected and
  user Dockerfiles alike.
- **Where builds run**: locally, `docker build` (BuildKit) on this host, at most
  `ROLL_APPS_BUILDS` (2) at once per node; more wait their turn, saying so in their log.
  BuildKit takes no per-build CPU or memory limit through `docker build`, so the cap on
  concurrent builds is what bounds them on one node. On the
  fleet, a build is customer code and never runs on a node: it is an AWS CodeBuild build
  (serverless, per-minute, privileged BuildKit inside CodeBuild's own VM) that pushes to the
  service's ECR repository (`<ROLL_APPS_ECR_REPO>/<service>`, made on the first build), behind
  the same `Builder` trait (`runtime/codebuild.rs`). CodeBuild was chosen over Kaniko in Fargate
  (Kaniko is unmaintained since 2025 and slow on big layers) and over BuildKit in Fargate (needs
  privileges Fargate doesn't grant).
- **A CodeBuild build**: the context goes up as a zip (CodeBuild's S3 source takes zips;
  symlinks stay symlinks) to `<prefix>builds/<deployment>-<random>.zip` in the stack's bucket.
  `StartBuild` on `ROLL_APPS_CODEBUILD_PROJECT` takes it as the source with an inline buildspec
  roll writes: log in to ECR, then `docker buildx build` in a `docker-container` builder with
  the job's Dockerfile, `--cache-from`/`--cache-to type=registry,ref=<repo>:buildcache`
  (mode=max, ECR's OCI form, so a rebuild on any node reuses the layers), and `--push` of
  `<repo>:<deployment>`, the image the deployment keeps. `BatchGetBuilds` is polled until it's
  done and its CloudWatch log is followed into the build log; the build timeout is CodeBuild's
  timeout and a local deadline, and a cancel is `StopBuild`. Deleting a deployment's image is
  ECR `BatchDeleteImage` of its tag (the repository and `buildcache` stay). The zip is deleted
  when the build ends.
- **Uploads** (`roll up`): the CLI sends what git would (`.gitignore`, a parent's too, as in a
  monorepo), less `.dockerignore` and `.rollignore`, and never a `.env` file other than
  `.env.example`/`.sample`/`.template`. rolld unpacks it under hostile-tree rules (roll-fetch:
  plain relative paths, no hard links, nothing written through a symlink, names and link targets
  at most 4 KiB read without trusting a header's size, PAX refused) and deletes it once its build
  ends; a sweep removes those of deployments that never built. A build root or Dockerfile is
  reached only through real directories, never a symlink in the tree.
- **Lean Node images**: after the build, devDependencies are pruned (`npm prune --omit=dev`,
  `pnpm prune --prod`), as Heroku's Node builds do, unless the start command uses one (`ts-node`,
  `nodemon`: the plan says so) or only a framework's output directory is copied. An express app
  with TypeScript and ESLint as devDependencies: `node_modules` 45 MB → 4.4 MB, the image 381 →
  332 MB (the rest is `node:22-slim`). A Next.js app whose `next.config` sets
  `output: "standalone"` ships only `.next/standalone`, `.next/static` and `public`, started with
  `node server.js` (a real Next 15 app: `/app` is 53 MB, its page and `public/` answer).
- **Variables in builds**: every install and build step sees the environment's variables
  (`NEXT_PUBLIC_*`, `VITE_*`, a Prisma `DATABASE_URL`, an `NPM_TOKEN`), as Railway's builds do.
  They're BuildKit secrets mounted as environment variables (`RUN --mount=type=secret,id=K,env=K`),
  so no value is in a layer, the image's history or the build cache's keys; BuildKit masks them
  in the build log. Each stage declares `ARG ROLL_BUILD_ENV`, a hash of the values, so a changed
  value rebuilds what read it and an unchanged set keeps the cache (`ROLL_DEPLOYMENT` and `PORT`
  stay out). On Docker each value is a secret read from a 0600 file, and the `docker` CLI runs
  with a cleared environment (how to reach the daemon, nothing else), so no variable steers a
  process of roll's (`LD_PRELOAD`, `BASH_ENV`, `DOCKER_HOST`). A tree's own Dockerfile also gets
  the build arguments its `ARG` lines declare, minus names that steer a process
  (`runtime::steers_a_process`), each set for that one `docker` command and never on argv. Docker
  records an `ARG`'s value in the image's history where a `RUN` uses it, so a secret belongs in
  `RUN --mount=type=secret,id=NAME,env=NAME`, which every variable is too. In CodeBuild
  the values are in a Secrets Manager secret of the build's own
  (`roll-apps/builds/<service>-<deployment>-<random>`, a JSON object), and each variable is an
  environment override of type `SECRETS_MANAGER` naming `<secret>:<NAME>`: no value is in the
  `StartBuild` request, the buildspec or the build's record, and the secret is deleted without
  recovery when the build ends. `CODEBUILD_*` and `AWS_*` aren't passed there either.
- **Private registries** (`PUT …/registry`, the settings page): an image source can pull with a
  login (a username and a password or token), sealed like variables and never returned. It goes
  to dockerd with each pull only (`X-Registry-Auth` on the Engine API), never `docker login`,
  which would keep it in the host's docker config for everyone. An image on a registry on this
  host or a private network (loopback, RFC 1918, link-local, unique-local, `localhost`) is
  refused: dockerd would fetch it on a tenant's say-so, and so is one whose name resolves to such
  an address. The login goes only to the registry the image names (a source moved to another
  registry forgets it), and New service (`registry` on create) takes it before the first pull.
- **Public images** skip the build: the deployment pins the image's digest at deploy time, so a
  moving tag never changes a running deployment.
- Build logs stream line by line to the dashboard and `roll deploy` as they're written
  (server-sent events), and are kept with the deployment.

### Deploys

- **One queue per service and environment.** A new deploy waits for the one in flight; a newer
  push supersedes queued ones that haven't started (`superseded`), as on Railway. Different
  environments deploy in parallel.
- **States**: `queued → building → deploying → active`, or `failed`, `canceled`, `superseded`;
  an active deployment becomes `inactive` when another replaces it. The dashboard maps them onto
  the shared mark states through `MarkStateProvider` (`deploying` while building or rolling out,
  then `success` or `error`).
- **Zero-downtime rollout**: new instances start next to the old ones, each must pass the
  health check (`GET <health_path>` answering 2xx–3xx, or the port accepting a connection when
  no path is set) within `health_timeout`; then the environment's pointer moves in one write,
  new requests go to the new instances, and the old ones drain (requests in flight finish, up
  to 30 s) and stop. A failed health check fails the deploy and leaves the old deployment
  serving. A service scaled to zero rolls out without starting anything: the pointer moves, and
  the next request wakes the new deployment (a health check runs at the first wake instead, and
  failing it there rolls the pointer back automatically).
- **Environments**: production comes with the service; more are made from the switcher (a
  name, the branch that deploys to it, and the environment whose variables it starts with:
  `variables_from`) or `roll apps env`. An environment's Settings panel changes its branch,
  makes it private (a fresh 32-character token from the browser's CSPRNG; the link is shown
  once, only its hash kept) or public again, promotes another environment's live deployment to
  it, and deletes it (production stays). **Promotion** (`POST …/promote {from}`, `roll apps
  promote --from staging`, Railway's) deploys the image `from` runs, with no build or pull, under
  the target's own variables and the service's current settings: staging's tested image goes to
  production as it was tested.
- **Who deployed** (`Deployment.actor`): a push names its commit's author as `git:<name>` (what the commit claims, shown as such; it can't pass for a token or `operator`); the API, the CLI and
  the dashboard name the caller rolld's gate saw (`operator` for the operator's token; on
  roll-mcp a scoped token's or connection's name, from `roll_auth::Grant`), never anything a
  client says about itself; deploys roll makes on its own (cron, a settings redeploy it starts)
  name nobody. The deployments list, `roll apps deploys` and notifications ("deploy failed … by
  Ada") show it.
- **Health checks while running** (Render's, Fly's `checks`): with a `health_path`, every
  running web instance is checked every 10 s (`LiveConfig::health_every`); three failures in a
  row (`health_failures`: an app that hangs or answers errors with its port still open) replace
  it: out of rotation, stopped, counted as a crash (so a loop of them notifies), and the next
  request or `min_instances` starts a fresh one. Each failure is a line in the environment's
  log. Without a path, a port that stops answering is noticed by the proxy and the reaper.
- **One-off jobs** (`POST …/jobs`, `roll apps run -- <cmd>`): a command in a fresh container of
  the active deployment, with its variables on the project network (no port, no volume: the
  service's instance may have it), as Render's one-off jobs and `heroku run` are. Its output is a
  log of its own (followed over SSE, kept like build logs); it ends at its exit, `timeout_s` or a
  cancel, and its container goes. `roll apps run` exits with the command's exit code. A node
  that takes the service over marks jobs a lost node was running as failed. With `tasks`
  (`roll apps run --tasks N`, Cloud Run Jobs'), it's N containers, ten at a time, each with
  `ROLL_TASK_INDEX` (0…) and `ROLL_TASK_COUNT` to split the work by; their output is the job's one
  log, each task's end a line in it, and the job fails as its first failing task does.
- **A shell** (`GET …/shell`, a WebSocket; `roll apps ssh`), as `railway ssh` and
  `fly ssh console`: `bash` (else `sh`) in a running instance through a TTY exec over the Engine
  API, the terminal raw on the CLI's side and resized with the window. A web service asleep is
  woken and held awake while the shell is open; a worker's running instance is used; a cron
  service has none (a job does that). It needs Docker on a local socket. The dashboard's Shell
  tab is the same shell over HTTP sessions (a browser can't send the token on a WebSocket): the
  screen over SSE with its scrollback, keys and resizes as POSTs, at most 8 open per
  environment, closed on leaving the page or after 10 idle minutes.
- **Single-page apps**: a static build (served by busybox httpd) whose `package.json` has a
  client-side router (`react-router-dom`, `@tanstack/react-router`, `vue-router`, …) is a
  single-page app (`Plan.spa`): roll's proxy answers a GET for a page (HTML, no file extension)
  that the app 404s with its `/index.html` and a 200, so deep links work (Vercel's and Render's
  rewrite). A missing file (`/logo.png`) stays a 404. `build.spa` or `roll.toml` `[app] spa`
  forces it either way.
- **JS workspace monorepos**: a service whose root directory (`apps/web`) is a package of a
  pnpm, npm or yarn workspace at the repo's root (`pnpm-workspace.yaml`, or `workspaces` in the
  root `package.json`) builds with the whole repo as its context, as Vercel and Railway build
  one: the install is the workspace's, once at the root (the lockfile and `packages/*` are
  there), the build runs the app's `build` script through the manager's workspace command
  (`pnpm --filter <name> run build`, `yarn workspace <name> run build`,
  `npm run build --workspace <dir>`), and the start runs its `start` script in its directory
  (`cd apps/web && exec npm run start`: npm ships in the image, so nothing downloads at a cold
  start; Yarn Berry's Plug'n'Play uses yarn). A command set in the service or `roll.toml` wins,
  and a root with its own Dockerfile keeps the root as its context. The plan (`POST
  /v1/apps/plan`) says so in its notes.
- **Watch paths** (monorepos; `build.watch`, `[app] watch`, the settings page): a push deploys a
  repo service only when a path that differs between what the environment runs and the pushed
  commit matches one of its patterns (gitignore-style from the repo's root, roll code's
  CODEOWNERS matcher; roll.toml's are read at the pushed commit). The environment's log says
  which pushes it skipped. Nothing set: every push deploys, as before.
- **Pre-deploy command** (`run.pre_deploy`, `roll apps set --pre-deploy`, the settings page):
  once per deploy, after the image is ready and before anything starts, in a container of the
  new image with the deployment's variables on the project network (not the volume: the old
  instance may still have it), as Railway's pre-deploy command and Render's do. Its output is in
  the deploy log; a non-zero exit, a timeout (the build timeout) or a cancel fails the deploy, and
  the old one keeps serving. For migrations, which must suit both the old and the new code.
- **Redeploy** (`POST …/redeploy`, `roll apps redeploy`, the dashboard's banner after a change
  of variables or settings): the active image again with the service's current run settings and
  variables, same revision, no build. It's how a `roll up` service takes new variables.
- **Retries**: `POST …/deploys` takes an `Idempotency-Key`; a retry answers the first call's
  deployment.
- **Instant rollback**: the old image is still in the registry (deployments keep their
  images until the deployment is deleted from history), so a rollback is a rollout with no
  build: seconds, not minutes.

### The runtime: scale to zero, wake on request

- **What a visitor sees when roll answers instead of the app**: one small page with the Horizon
  mark (`assets/horizon.svg`, `@roll/brand`'s export, a test keeps it equal), dark or light as
  their system is: "Starting up" or "Restarting" (503, `Retry-After`, and it reloads itself after
  5 s), "Paused" (402, the project's spend limit), private (401), not deployed (404). An app's
  failure (a connection closed before it answered) is a 502 that says so without the error, which
  goes to the environment's log.

- **The waking proxy** (Knative's activator, Fly's proxy) runs on the service's owner node. A
  request for a service with no running instance is held (up to `wake_timeout`, 60 s, then a
  503 with `Retry-After`), one wake starts, and every held request goes through as soon as the
  instance answers its port. Requests arriving while it wakes join the same wait; nothing
  starts twice.
  - The wake runs on its own task, so a client that goes away mid-wake leaves nothing half
    started. At most 1,000 requests wait per environment (more get a 503 with `Retry-After`),
    and after a failed wake the requests behind it get its error for a backoff (1 s doubling to
    30 s) instead of each trying again. Calls to Docker have timeouts, so a hung daemon fails a
    wake instead of holding it.
  - An instance that dies on its own (killed, out of memory) is noticed by the next reaper tick,
    or by the proxy when its port refuses a connection (a 503 with `Retry-After: 1`); the next
    request starts a fresh one.
  - A service with a volume has one writer: a rollout stops the old instance and keeps the
    environment locked until the new one is live, so requests wait for the new deployment
    instead of starting the old one on the volume.
  - Deleting an environment deletes its row first, then tears down its live state, which
    refuses to make or adopt anything afterwards. A node that loses a service's lease cancels its
    deploys in flight and doesn't switch an environment it no longer owns; on a shared cluster
    backend the switch is fenced too (`fences`: the service's lease epoch, checked in the
    switch's own transaction), so an older owner finishing late can't move the pointer.
- **Idle states**, cheapest last:
  1. **running**: serving, billed per second of CPU and memory it actually uses.
  2. **suspended**: after `suspend_after` (default 30 s) without a request, the instance is
     frozen (Docker `pause`, the cgroup freezer: memory kept, no CPU). Resuming takes
     milliseconds. Not billed to the user; roll pays for the memory, so it lasts only until
     `sleep_after`.
  3. **stopped**: after `sleep_after` (default 10 min) the instance stops. Its container stays
     created, so waking is a start of the app, not a pull or a create. This is the cold start.
  An HTTP service with `min_instances: 0` reaches stopped; `min_instances ≥ 1` keeps that many
  running (no cold starts, billed while up).
- **Why tiered** and not one idle timeout: most wakes come soon after the last request
  (a user clicking around, a webhook burst), and a frozen process answers them in
  milliseconds; the long tail of idle time costs nothing because it ends stopped. This is Fly's
  suspend/stop split, done with the freezer locally.
- **On the fleet** (`ROLL_APPS_RUNTIME=fargate`, `runtime/fargate.rs`), instances are Fargate
  tasks, with the isolation of docs/cloud/README.md's "Isolated work" but no template:
  - A task definition per deployment image, registered by roll and reused: family
    `roll-apps-<service>-<hash>` (the hash over the image, the size, the port, whether a start
    command replaces the image's own, the roles, the log group, the architecture, the guard and
    its idle). `app` has the image's own entrypoint (or `sh -c <start>`), `awslogs` to
    `ROLL_APPS_FARGATE_LOG_GROUP`, the execution role, and a task role only if one is set (none
    by default: no AWS credentials in the task). A web task also has `guard`, roll's small image
    (`roll-fetch proxy`, `ROLL_APPS_FARGATE_GUARD_IMAGE`): the only port nodes reach is its
    `ROLL_APPS_FARGATE_PORT` (the stack's PreviewPort, 9970, the one the isolated subnets'
    security group and network ACL admit, as for previews), forwarded to the app's own port
    over the task's shared localhost; it ends the task once nothing has moved for well past the
    service's `sleep_after` (never for `min_instances` above 0), so a task whose node went away
    still stops. `create` registers or finds it and runs nothing.
  - `start` is `RunTask` (awsvpc, `ROLL_APPS_FARGATE_SUBNETS` and `_SECURITY_GROUP`, on demand,
    `startedBy roll-apps-<service>`, tags `sh.roll.apps=<service id>` and
    `sh.roll.apps.instance=<name>`). It waits for RUNNING and answers the task's private address
    on the guard's port, which the owner's proxy health-checks and forwards to as it does a
    container's. `stop` is `StopTask` (SIGTERM, then SIGKILL after the task definition's 30 s
    `stopTimeout`), and the next start is a new task. `wait` polls `DescribeTasks` to STOPPED and
    answers the `app` container's exit code (cron runs, jobs, pre-deploy commands). `list` is
    `ListTasks` by `startedBy` with tags, so a node that takes a service over finds an earlier
    owner's tasks and stops them.
  - **Variables never go in a `RunTask` request** (it shows in `DescribeTasks` and CloudTrail).
    Each start writes them to an object of its own, `<prefix>env/<instance>-<random>.env` in the
    stack's bucket, and the request names only its ARN as the `app` container's environment
    file. ECS reads it with the execution role before the app starts, and roll deletes it as
    soon as the task is RUNNING (or stopped, or failed to start). This is code-preview's bundle
    with ECS as its `fetch`. It was chosen over a `fetch` container because an app keeps its
    image's own entrypoint (sourcing an env file needs a shell, and distroless images have none)
    and no roll image goes in the task. An environment file is one line per variable, so on the
    fleet a value with a line break (a PEM key) is refused at deploy; base64 it.
  - Public images are pinned by a manifest `HEAD` on their registry (anonymous, or with the
    registry's anonymous token), through a client that refuses private addresses.
  - Fargate can't freeze a task, so there the states are running → stopped (`can_suspend` is
    false, and `settle` does nothing: no CPU boost, the size is the task's). A cold start is a
    task launch (tens of seconds for a large image; SOCI indexes cut the pull), so services that
    must answer instantly set `min_instances: 1`. A warm window (a task kept up a few minutes past
    `suspend_after`, unbilled) isn't built: an idle task stops at `sleep_after`. The fast path on
    the fleet is in `## Next`: Firecracker microVM hosts with snapshot restore (what Fly and
    Lambda SnapStart do), behind the same `Runtime` trait.
  - Metering: `usage` answers nothing on Fargate (a task's cgroup isn't read). Fargate bills size
    × time, so the meter charges the allocation while an instance runs (its CPU × seconds and
    its memory).
  - Settings (`runtime::aws::FleetConfig::from_lookup`): `ROLL_APPS_RUNTIME=fargate` (default
    `docker`), `ROLL_APPS_FARGATE_CLUSTER`, `_SUBNETS` (comma separated, the isolated subnets),
    `_SECURITY_GROUP` (the isolated one), `_PORT` (9970), `_GUARD_IMAGE`,
    `_EXECUTION_ROLE`, `_TASK_ROLE` (optional), `_LOG_GROUP`, `_PUBLIC_IP` (`1`), `_ARCH`
    (`ARM64` or `X86_64`), `ROLL_APPS_CODEBUILD_PROJECT`, `ROLL_APPS_ECR_REPO` (`apps`),
    `ROLL_APPS_BUCKET` (default `ROLL_S3_BUCKET`), `ROLL_APPS_PREFIX` (`apps/`) and `AWS_REGION`.
    Unset, rolld runs and builds on Docker as before.
- **Autoscaling**: the owner counts requests in flight per instance every second; the target is
  `concurrency` (default 50) in flight per instance. Instances needed = ceil(in flight /
  concurrency), between `min_instances` and `max_instances`, scaling up at once and down only
  after 60 s below target (no flapping). Workers run exactly `min_instances` (at least 1).
- **Workers** have no port and no proxy: they run `min_instances` (default 1) and restart on
  exit with backoff (1 s doubling to 60 s).
- **Cron jobs** are a service type with a schedule (five-field cron, in `run.timezone` — IANA, its daylight saving kept, Cloud Scheduler's — or UTC). Each run is a fresh
  instance of the active deployment that runs to its exit, with a timeout (default 1 h) and its
  output as a log; a run that is still going when the next is due is skipped (Railway's rule).
  The schedule lives with the service and fires on the owner node; roll tasks wasn't used for
  this because a cron run must start an instance on the owner (a runtime action, not a webhook
  delivery), and keeping the schedule next to the deployment keeps a rollback's schedule right.
- **Resources**: `cpu` (0.25–8 vCPU) and `memory_mb` (256–16384) per instance, as Docker
  `--cpus`/`--memory` (no swap) and a pids limit, or the Fargate size rounded up (the default
  0.5 vCPU and 512 MB is a 0.5 vCPU, 1 GB task).
- **Volumes**: a service may have one volume (a mount path and a size). Locally it's a named
  Docker volume `roll-apps-vol-<service>-<env>`; a service with a volume runs at most one
  instance (`max_instances` is forced to 1, as on Railway). On the fleet it's an EFS access
  point per service and environment (in `## Next`).

### Isolation

- Customer code never runs on a fleet node: instances, cron runs and builds are Fargate tasks
  and CodeBuild builds. Locally they're Docker containers with no host mounts (sources are
  copied into the build context, never bind-mounted), CPU and memory limits (memory without
  swap), a pids limit, `no-new-privileges`, Docker's default seccomp/AppArmor profiles, and the
  capabilities nothing here needs dropped (`NET_RAW`, `MKNOD`, `AUDIT_WRITE`, `SETFCAP`). Disk
  isn't capped locally: a writable-layer quota (`--storage-opt size=`) and `volume.size_gb`
  need overlay2 on XFS with project quotas, which a plain ext4 host doesn't have; on the fleet
  Fargate's ephemeral storage bounds it.
- Variables reach a container as values of the Engine API's create call: never argv, never a
  file, and multi-line values (PEM keys) work. Only over the CLI (a daemon that isn't a local
  socket) do they go through a 0600 env file, removed right after the create; that path refuses
  a value with a line break.
- Every container, network, volume and image roll apps makes is named `roll-apps-…` and carries
  the label `sh.roll.apps=<service id>`, so leftovers are found and removed by exact name.
- A service's variables reach only its own instances. They're stored sealed
  (`ROLL_SECRET_KEY`, AES-256-GCM; locally a key file under `<home>/apps/key` made 0600 on first
  start), never logged, and never in a `RunTask` or `StartBuild` request (a task's come from its
  own environment file, a build's from its own Secrets Manager secret, each deleted once read).
  They're returned in the clear only by the `reveal` route.

### Networking

- **Public URLs**: every service in an environment gets
  `https://<service>-<project>[-<env>].up.roll.site` (production omits the env, and the project
  is left out when it has the service's name, as `roll up` names both; a taken name
  gets a 4-character suffix). `up.roll.site` keeps apps off the bucket namespace
  (`<bucket>.roll.site`) and on roll.site, which only serves user content. Cloudflare proxies it
  (`*.up.roll.site` needs Advanced Certificate Manager on the zone, a fixed $10 a month; the
  origin has the wildcard from ACME DNS-01 like the other site names). Locally the domain is
  `apps.localhost` on the apps listener (`ROLL_APPS_LISTEN`, `127.0.0.1:7080`), and each service
  can also get a port of its own (`ROLL_APPS_PORTS=58100-58199`) so it opens on a machine without
  wildcard DNS (a Tailscale name).
- **Notifications** (`…/projects/{project}/notifications`, a service's Settings tab): a project's
  failed deploys and crash loops (and, when asked, every new live deployment) go to its targets:
  a webhook (JSON, signed with HMAC-SHA256 over `<timestamp>.<body>`), a Slack incoming webhook
  or a Discord webhook. One node (the singleton `apps:notify`) follows the event log after a
  cursor in `state`; a page of events' deliveries and the cursor's move are one transaction, so
  an event is queued once and never skipped. A delivery is tried at once, then after 30 s, 2 min,
  10 min, 30 min and 1 h, then marked failed; each target keeps its delivery log, and Test sends
  one now. URLs are sealed (a Slack URL is a credential), HTTPS, fetched with public addresses
  only and no redirects (`apps_control::net`, which the Fargate runtime's registry client uses
  too), answers read bounded; Slack must be `hooks.slack.com`, Discord `discord.com/api/webhooks`.
  - **Why this, not roll code's webhooks or chat ops** (decided 2026-11-13; reversible): roll
    code's delivery queue is per repository, and an upload or image service has none; chat ops
    drives roll from chat through installed apps and bound threads, more than an alert needs,
    and a Slack or Discord incoming webhook needs no install. Railway and Render notify the same
    way. The event log is the seam: a platform-wide notifications product (email, roll code's
    owner-level webhooks, chat ops) reads the same cursor, and this deliverer goes then.
- **The event log** (`GET /v1/apps/events?after=`): `deploy.active` (written in the switch's
  transaction), `deploy.failed` (in the failure's), `crash_loop` (once per loop, by the reaper),
  in order, kept 7 days, for anything that must not miss one (roll code's webhooks, chat ops, an
  MCP client) to read after its cursor. Sequence numbers come from a counter row updated in the
  event's own transaction, not an identity (taken before commit, so on Postgres a slow
  transaction could land an older number after a reader moved past it): they commit in order
  with no gaps.
- **Crash loops**: every crash (an exit, a start that fails or never answers, a port that stops
  answering) is counted per environment, with why: the runtime's exit code and whether the kernel
  killed it for its memory ("it ran out of memory (its limit is 256 MB)", from the Engine API's
  `State.OOMKilled`/`ExitCode`, or Fargate's container `reason`/`exitCode`); the Memory chart
  draws the limit; three in ten minutes is a crash loop, said once in the
  environment's log and on `Environment.crash_loop` (the count, when, and why), which the
  service page shows as a banner with its logs a click away.
- **Private environments** (previews): an environment with an access token answers only
  visitors who bring it: `?roll_access=<token>` once sets a host-only `HttpOnly; SameSite=Lax`
  cookie and redirects to the URL without the token; a request without it is a 401 that wakes
  nothing. Only the token's SHA-256 is stored, compared in constant time, and the cookie is taken
  out of what the app receives. roll previews sets it on every `pv-…` environment. An automation
  (a CI run's end-to-end tests) can be let in with the project's bypass secret, as Vercel's
  protection bypass does: `X-Roll-Preview-Bypass: <secret>` on the request, only its SHA-256
  stored (`bypass_sha256`), the header never reaching the app.
- **Custom domains**: the user adds `www.example.com`, points a CNAME at the service's
  `up.roll.site` name, and roll verifies it (the CNAME, or a `_roll-challenge.<host>` TXT with the
  domain's token for apex domains behind an ALIAS/flattening provider), checking pending ones
  every minute. Domain states: `pending` (waiting for DNS) → `active`, or `failed` with the
  reason. A wildcard, `*.example.com` (Railway's and Render's), serves every name one label
  under it that no exact domain claims: a CNAME on `*.example.com`, and the TXT proof on
  `_roll-challenge.example.com`. Its certificate needs DNS-01, so where custom domains get
  certificates and they can't be wildcards (`DomainTls::wildcards`, the edge's HTTP-01 today)
  it's refused at once rather than left pending; on a node serving plain HTTP it just works. A domain still unverified 7 days after it was added is removed (`PENDING_CLAIM_TTL`), so
  a squatted hostname comes free for its real owner; one verified once never expires. Removing an
  environment or a service removes its domains (rows and provider records alike). Where the CNAME
  target is shared by every tenant (`cloudflare` below: `up.roll.site`), a CNAME proves nothing
  (a hostname someone already points there, or left pointing, would go to whoever claims it
  first), so `Domain.record` is `both` and only the TXT challenge activates it. A service holds at
  most 50 custom domains and a project 200 (`MAX_DOMAINS_PER_SERVICE`, `_PER_PROJECT`):
  certificates and their orders are shared, and the edge's pass over active hostnames takes turns
  (one past its `max_domains` waits a pass, never loses its certificate). A custom domain can redirect
  to another of the environment's hostnames (`PATCH …/domains/{host} {redirect_to}`, the Domains
  tab): a 308 with the path and query, as Render does for www and apex; it can't point at
  itself, at one that redirects, or redirect while another points at it.
- **Custom domains' certificates** (`Domain.tls`: `none`, `pending` → `issued`, or `failed`, with
  `tls_error`): one seam, `integrations::DomainTls`, chosen by `ROLL_APPS_TLS`:
  - `cloudflare` (the fleet): each domain is a **Cloudflare for SaaS** custom hostname on the site
    zone (`roll_edge::saas`: `POST /zones/{zone}/custom_hostnames`, HTTP DCV, a DV certificate),
    created when roll apps accepts and stores the domain, never before; verifying (and the
    minute loop, until it's issued) reads its state; removing the domain deletes it. Customers'
    CNAME target is the zone's fallback origin `up.roll.site`. Cloudflare issues and renews at its
    edge and connects to the origin with the custom hostname as the server name; the edge answers
    names it has no certificate for with the fallback origin's (`CertStore::set_unknown_name`).
    Answers are read at most 1 MiB, and only `[a-z0-9.-]` hostnames outside roll's zones are sent.
  - `acme` (a node alone, no Cloudflare): the edge orders a certificate per **active** custom
    domain over ACME HTTP-01 (`roll_edge::domains::DomainCerts`, on the `acme` singleton like the
    edge's own sets). It learns the domains through `roll_edge::domains::CustomDomains`, which
    rolld implements over `Apps::active_custom_domains` and `Apps::set_domain_tls` (apps-control
    and roll-edge don't depend on each other). The plain-HTTP listener (`ROLL_EDGE_HTTP`, port 80
    from outside) answers `GET /.well-known/acme-challenge/<token>` for any host from the
    in-memory tokens of the orders in flight, and redirects everything else to HTTPS. Each
    certificate is stored sealed (`system/certs/domains/<host>.sealed`), served by SNI, renewed 30
    days before it expires, and uninstalled and deleted when the domain goes. At most 5 orders a
    pass (every 30 s); a failing hostname waits 5 minutes, doubling to a day. The ACME account
    (`ROLL_ACME_DIRECTORY`) is the one the edge's DNS-01 certificates use.
  - `none` (the default): `tls` is `none`; locally services are plain HTTP on the apps listener.
- **Private networking**: a project's services reach each other at
  `<service>.<env>.internal:<port>` and by bare `<service>`. Locally every project and
  environment has a Docker network `roll-apps-net-<project>-<env>` and each instance joins it
  with its service name as an alias. On the fleet, tasks of one project and environment share a
  security group that admits only itself, and names resolve through Cloud Map (in `## Next`).
- **A private caller wakes a sleeping service** (Railway's private network, Fly's Flycast).
  rolld never sees that traffic, so it reads the instance's own network numbers instead
  (`runtime/netns.rs`, the sandbox's `/proc/<pid>/net/…`, readable by anyone and still while the
  app is frozen). Cheaply: TCP `PassiveOpens` in `snmp` counts the connections the namespace
  accepted (34 µs a read), marked when the instance goes to sleep and asked five times a second.
  The socket tables (`tcp`, `tcp6`) would say who's connected, but for another namespace the
  kernel walks the whole host's table: 5.6 ms a read on ai-server, so the first version, which
  read them five times a second per sleeping instance, would have spent a core on 36 of them.
  They're read now only where nothing else tells: a running instance about to sleep (at most
  every 5 s), and arming a waker (a few reads, off any request's path):
  - Frozen (suspended): a connection in the app's backlog resumes it (about 100 ms more than
    an awake call).
  - Stopped: a waker in the sandbox (`<instance>-wake`, busybox `nc` with no capabilities, a
    read-only root and 16 MiB) listens on the app's port. Accepting the first caller closes its
    listener, so rolld sees a connection with nothing listening and starts the app, and the
    waker joins the held caller to the app once it listens. The first call is answered, not
    refused.
  - Before any other start (a public request, a cron run) the waker lets go of the port: rolld
    calls it once through the published port, which frees it in a millisecond (killing its
    container took Docker 70), and the app's bind never races it.
  - Running: connections from peers other than the host (rolld's own come through the
    network's gateway) count as use when it would otherwise sleep, so a service in use only
    privately doesn't. An idle pooled connection counts too, as on Railway.
  - Measured on real Docker (`a_private_caller_wakes_a_sleeping_service`, a caller on the
    project network with `docker exec … wget`, which is about 90 ms of the time): awake 86–115
    ms, stopped 281–457 ms, frozen 185–214 ms. A public cold start with the waker armed was
    167–210 ms, and five alternating pairs with and without the waker (sandbox medians 165 and
    200 ms against 175 and 212) showed no cost.
  - Not on the fleet yet (Cloud Map, in `## Next`). A second caller arriving while the app
    starts from stopped is refused (the waker holds one); the app's port must listen on
    loopback too (`0.0.0.0` or `::`, as every generated start command does).
- **roll databases**: a service links to the project's Postgres, Redis, buckets, streams,
  queues and NoSQL. A link injects the resource's connection variables (`DATABASE_URL`,
  `REDIS_URL`, `S3_*`, `KAFKA_*`, `AMQP_URL`, `CASSANDRA_*`), named with the resource's prefix
  when a service links two of a kind (`ORDERS_DATABASE_URL`). Locally the URLs are rewritten for
  the container (`host.docker.internal`); on the fleet they're the public edge names over TLS,
  as previews use, so there's no path from a task into the fleet's network.
- **Service links** (Render's `fromService`, Railway's reference variables): a `service` link
  to another web service of the project gives `<NAME>_URL`, its private
  `http://<name>.<env>.internal:<port>` in the linking environment, and `<NAME>_PUBLIC_URL`
  when it's public there. They're read at each deploy (a renamed service's new name, a new
  port), checked to be the same project each time, and the links to a deleted service go with
  it. The canvas draws them like a database's. A scoped token needs `reveal` on the linked
  service, as on any linked resource.
- **Credentials per link**: a kind whose product can make a credential per consumer does so
  once, when the link is made (`Resources::link`: a roll search API key, a native user for
  OpenSearch), instead of the resource's own password. Its variables are kept sealed with the
  link (AAD names the service, kind and resource) and are what deploys and listings read;
  setting the link's prefix again keeps the same credential. Unlinking, or deleting the service,
  revokes it (`Resources::unlink`; a revoke that fails is logged, and the link is gone either
  way). A link that can't be stored revokes what it just made. Kinds that don't (Postgres and
  Redis today) read the resource's variables fresh at each deploy.

### Secrets and environments

- **roll secrets** (feature/roll-secrets, docs/secrets.md) owns per-project secrets per
  environment. roll apps reads a deploy's secrets through one port,
  `integrations::SecretSource::resolve(project, env, service)`, and never stores a second copy
  of a project secret. Until #20 is on main `NoSecrets` stands in.
- Service variables per environment (Railway's service variables), sealed at rest, are roll
  apps' own: a service's config (`PORT`, `NODE_ENV`) belongs to the service; project secrets
  come from roll secrets and service variables win on a clash.
- The wiring (`## Next`): creating an apps environment calls `Secrets::ensure_environment`
  (idempotent; a preview's `pv-<id>` is previews' to make, with its base); apps never deletes a
  secrets environment; a missing one means no secrets; `needs_deploy` compares
  `Secrets::digest` instead of resolving values; `Values.linked` says which addresses to rewrite
  for containers. Previews' environments are untrusted in roll secrets itself (an ephemeral
  environment not made `trusted` never reads a protected one), so the port carries no trust flag.
- What a deploy injects, lowest to highest precedence: roll's own (`PORT`,
  `ROLL_SERVICE`, `ROLL_ENVIRONMENT`, `ROLL_DEPLOYMENT`, `ROLL_PUBLIC_URL`,
  `ROLL_PRIVATE_HOST`), links, the environment's project secrets, the service's variables.
- A deployment records the variables' names and a hash of their values: changing a variable
  marks the environment "needs deploy" (like Railway's staged changes). When a deployment goes
  live, the environment's own variables are kept with it, sealed under the deployment
  (`deploy_vars`), and a rollback to it brings them back as they were then (Heroku's and
  Railway's rollbacks: a bad variable change is undone by rolling back), the change in the
  activity as `variables.restored`; `keep_variables` (`roll apps rollback --keep-variables`) runs
  its image with today's. Links' and roll's own variables are resolved again, as at any deploy;
  a deployment that never went live (a no-traffic try-out) has none kept.

### Preview environments

The previews thread (feature/roll-previews) drives roll apps through two calls, both on the
public contract so they need no private hook:

- `PUT /v1/apps/services/{id}/environments/{env}` with `{ "branch": null, "ephemeral": true }`
  makes the environment (an ephemeral one is never the target of deploy-on-push and is
  listed apart), then `POST /v1/apps/services/{id}/environments/{env}/deploys` with
  `{ "revision": "<sha>" }` deploys that revision into it.
- `DELETE /v1/apps/services/{id}/environments/{env}` tears it down: instances stopped and
  removed, domains released, variables deleted, deployments kept in history as `removed`.
- Preview environments scale to zero like any other, so an open pull request costs nothing
  until someone opens its URL.

roll previews' feedback button works on a preview environment of roll apps too, when roll
previews turns it on for the project (`feedback` on the environment): a visitor who came in
through the preview's access link (its cookie; an automation's bypass header doesn't count) gets
`<script src="/__roll/feedback.js" defer>` before `</body>` of a page load (a browser's document,
else a GET that asks for HTML), the page asked of the app uncompressed and sent `private,
no-cache` with no length or ETag; the script itself comes from the preview's own host. It holds no
credential: notes go to the preview in roll's dashboard. The button also lists what's been said
about the page (Vercel's comment pins): `GET /__roll/feedback.json?path=<pathname>` on the same
host and for the same visitors, answered from roll previews through a port
(`integrations::FeedbackNotes`, `Apps::set_feedback_notes`) as previews' own proxy answers it:
`no-store`, the latest 200, a 404 where previews isn't on the node and a 502 when it fails (the
button then shows no list). The script and the tagger are roll previews', copied
(`assets/feedback.js`, `feedback.rs`, equal to #28's) until one copy serves both.

### Permissions and MCP

roll apps adds no token or scope checks of its own. Its routes sit behind rolld's gate
(`crates/roll-auth`; scoped tokens and the catalogue in `crates/roll-access` on
feature/roll-mcp), and every route is classifiable from its pattern alone:

- the service id is in the path, and the environment too wherever an action targets one;
- production is the environment name in the path, so `deploy` (deploy, rollback, promote to
  production) is separable from `write` (deploy to a non-production environment) by the route;
- revealing variables is its own route (`…/variables/values`), so `reveal` is separable from
  `read`;
- deleting a service, an environment or a domain is a `DELETE` on its own path.

The product name is `apps`; the actions are `read`, `write`, `deploy`, `delete`, `reveal` and
`admin`. The route table with each route's action is in http-api.md, "Routes and actions".

### On the fleet

- Ownership: lease `apps:service:<id>`. The owner runs the service's deploy queue, waking proxy,
  autoscaler, cron schedule and meter. Any node accepts a request for a service's hostname and
  hands it to the owner with `Cluster::dispatch` (WebSockets included). A node that takes a
  service over starts from the store: it marks instances the previous owner had as unknown and
  stops the Fargate tasks still running as the service's (`startedBy roll-apps-<service>`);
  the next request starts a fresh one. (Adopting them instead, after checking their deployment,
  would save that cold start; it isn't built.) Deploys in flight on a lost
  node fail and are retried once by the new owner (builds are idempotent: same revision, same
  image tag).
- Every catalog write that belongs to a service is a `code_sql` write closure; the switch of an
  environment's active deployment is a compare-and-swap on the previous pointer, so two nodes
  that briefly both think they own a service can't both switch it.
- The store is `code_sql`: SQLite at `<home>/apps.db` on one node, the system database's `apps`
  schema in a cluster.

## Observability and cost

- **Runtime logs**: each instance's stdout and stderr, line by line with the time, stream and
  instance, kept in memory per environment (the last 5,000 lines) and appended to log chunks in
  object storage (`apps/logs/<service>/<env>/<day>/<seq>.ndjson`, kept 7 days). `roll logs -f`
  and the dashboard follow them over server-sent events.
- **Metrics** per service and environment, per minute: requests, 2xx/4xx/5xx, latency p50/p95/p99
  (a fixed-bucket histogram at the proxy), CPU (cores used), memory (bytes resident), instances,
  cold starts and their duration (from wake to the first byte the instance answers). Kept 7 days
  at minute resolution in the store.
- **Metering**: usage per service and environment per hour: CPU-seconds used, GB-seconds of
  memory resident while running, requests, egress bytes, build minutes. Suspended and stopped
  instances meter nothing.
- **Prices** (roll cloud, per second of real use; the dashboard shows them and the month so
  far):

  | what | price |
  | --- | --- |
  | CPU | $0.000012 per vCPU-second used ($31 per vCPU-month flat out) |
  | memory | $0.000004 per GB-second resident ($10.4 per GB-month) |
  | egress | $0.05 per GB |
  | builds | $0.005 per build minute |
  | requests | free |

  An idle service costs $0.00: it is stopped, and a stopped instance meters nothing. A
  low-traffic site (a request a minute, 50 ms of CPU each at 100 MB) is about 1,440 s of
  running a day if each request keeps it awake 1 s, a few cents a month.
- **What roll pays for idle**: locally nothing but disk (images, stopped containers). On the
  fleet, the image in ECR ($0.10/GB-month) and the warm window's Fargate seconds.

## Request logs

Every request through the proxy is a line in the environment's access log (`http:<service>/<env>`,
`?http=1` on its logs route, the Logs tab's Requests view, `roll logs --http`), as Railway's HTTP
logs: method, path (no query string: it can carry tokens), status, milliseconds, bytes out, the
instance, the request id (`X-Request-Id`, made when the client sent none) and a cold start's wait.
`status` (`500`, `5xx`, `400-499`) and `min_ms` filter it, read or followed. It's apart from the
app's own output, so a busy service's requests don't push its logs out of memory.

## Metric alerts

Each minute the owner of an environment reads its last five finished minutes against the live
deployment's `run.alerts` (Railway's monitors, Heroku's alerts): memory at or above
`memory_pct` of the limit per instance in every minute (default 90), 5xx answers at or above
`error_pct` of at least 20 requests (default 5), p95 latency at or above `p95_ms` in three of the
minutes (default off). Each is one event (`alert.memory`, `alert.errors`, `alert.latency`) when it
starts, notified like a crash loop, and can fire again once it has cleared. Settings has the
thresholds, as does `roll.toml` `[app.run.alerts]`.

## Commit statuses and waiting for CI

A repo deployment reports on its commit as a roll code status (`roll/apps/<service>/<env>`,
mirrored to GitHub with the rest), pending while it's on its way and then success or failure,
linking to the environment, as Railway's and Vercel's GitHub deployments do. An environment with
`wait_for_ci` (the Environment panel, `roll apps env --wait-for-ci true`) deploys a push only
once its commit's checks pass (Railway's "Wait for CI", Render's "after CI checks pass"): the
statuses other than roll's deploy statuses (roll apps' own `roll/apps/…`, and roll previews'
`roll/preview`, which is about a preview environment, not the code). With none, it deploys at
once. One that's pending waits (a
newer push takes its place, as it would supersede the deploy; `Environment.waiting_for_ci` names
it), is settled by the landing follower every couple of seconds, and is dropped after an hour. A
failed one isn't deployed, which the environment's log says.

## Audit trail

Every change someone makes to a service is an event in the event log, `audit.<what>` with the
actor (as `Deployment.actor`: what rolld's gate saw): settings (the keys a PATCH set), variables
(the names set and removed), a reveal of the values, an environment's settings or deletion,
domains (added, removed, redirected), links and the registry login. Never a value: variables by
name, an access token as "access changed". `GET /v1/apps/services/{id}/activity` and the Activity
panel in Settings read it, newest first; audit events are kept 90 days (the rest of the log 7).

## Incidents

A crash loop's banner on the service page has the logs a click away and **Roll back** to the
deployment that was live before (its image again, no build); `roll apps rollback` with no id
does the same, as `heroku rollback`. The server picks the target (`POST …/rollback`): it follows
the `rollback_of` chain, so after B → A the next goes before A, never forward to B. A failed or
timed-out cron run is a `cron.failed` event,
notified by default; a failed one-off job is `job.failed`, off by default (someone ran it and saw
it). Both carry the exit code.

## Gradual rollouts

`run.rollout` (Vercel's rolling releases, Cloud Run's traffic splits): `steps` are increasing
percents (`[10, 50]`, at most five), each lasting `step_s` (default 120). After the new
deployment's instances pass their health check, they join the old ones instead of replacing
them, and the proxy sends each step's share of requests to them by turns (request *n* goes to
the new one when `(n × 37) mod 100 < share`, so each hundred requests sends exactly that many,
spread out). The proxy counts each deployment's answers and 5xx while a canary runs. The
rollout reads them every quarter of a step (5 s at most), and a step fails when the new
deployment answered `error_pct` (default 5) or more of at least 20 requests with a 5xx and more
often than the old one did. An outage both share, like a database that's down, isn't the new
code's fault. What no answered request shows counts too: a request routed to the new deployment
whose wake failed is one of its 5xx, and two or more crashes of its instances (a failed health
check, a process that exits) roll it back when the old one crashed less. The wake backoff is per
deployment, so a canary that won't start never holds back the old one's wakes, and each
deployment's instances are health-checked on its own path. A failure rolls it back: every request goes to the old deployment again, the new
one's instances drain, and the deploy fails with the numbers ("rolled back at 10% of requests:
14 of its 40 requests were 5xx…"). After the last step, the store switches the active
deployment (`switch_active`, the same compare-and-swap) and the old instances drain. Canceling
the deploy, or Roll back on the service page, does the same as a failed step. The environment
shows `canary` (deployment and share) while one runs, and each step is in the deployment's
steps (`canary_10`). A visitor stays on one deployment through a step (Vercel's rolling
releases): while a canary runs, the proxy gives a visitor without one a `roll_bucket` cookie
(0–99, a day) and sends buckets under the share to the new deployment; the cookie never reaches
the app. A client that keeps no cookies is routed by turns, as above.

A rollout splits requests, so it needs a web service whose old deployment is running, outside
a preview environment (a reviewer or two is nothing to judge a canary by). The first
deployment, an environment that's asleep, a worker and a cron service switch at once, and a
service with a volume refuses `steps` (one instance writes it). The state lives on the owner.
If the owner changes mid-rollout, the new owner runs the deploy again from the start, like any
other deploy it takes over.

## A project in one file

A root `roll.toml` can declare a whole project (Render's Blueprints), next to whatever else is in
it:

```toml
[apps.web]
root = "apps/web"          # uploaded by roll up; its own roll.toml [app] still applies
links = ["db", "cache"]
[apps.proxy]
image = "nginx:1.27"
port = 80                  # and [apps.<name>.variables]: both set when the service is made
[apps.jobs]
repo = "acme/shop"         # a roll code repo: deploys on each landing on `branch`
root = "jobs"              # with repo: the directory in it that builds
branch = "main"
type = "worker"            # web, worker or cron (with `schedule`), `start` too
[apps.jobs.run]
min_instances = 2          # any run setting, checked as the API checks them
[databases.db]
kind = "postgres"
[databases.cache]
kind = "redis"
```

`roll up --all` (the project is `--project` or the directory's name, made if missing) prints the
plan first (`+` made, `=` there, `~` linked, `↑` deployed; `--dry-run` stops there). A database
counts as there only when it's on the project's canvas (another project's `db` is never this
one's), and a service the file names with another kind of source stops it (renaming one is the
fix, never a quiet conversion). Then it makes the missing databases and places them on the project's canvas, makes the missing services with
their links so the first deploy has the variables, links what an existing service lacks,
uploads each `root`, and redeploys an image service whose image changed. Names are matched, so
running it again only does what changed, including the run settings an entry names (`~
settings`, only those that differ; the rest stay as they are). On a push there's nothing more
to sync: a repo service's own `roll.toml` `[app]` (in its root) applies over its settings at
every deploy, and every landing deploys, which is what Render's Blueprint sync does for an
existing service. Making new services and databases needs a project and other products, so
that stays `roll up --all`'s. Nothing is ever deleted: a service or database removed
from the file stays until removed by hand. The file is checked before anything is made: one of
`root` or `image`, a root inside the directory, links to declared databases, kinds `postgres`
or `redis`. `roll apps import-compose [file] [--write]` turns a Compose file into these entries
(Railway's Compose import): image and build services become `[apps]` (the container's port,
its environment as variables), Postgres and Redis containers become roll databases linked from
what `depends_on` them, and what isn't carried over (volumes, networks, commands, health checks)
is said. The plan, the checks and the import are unit-tested in roll-cli, and the whole run was
checked against a live rolld (apps and Redis on Docker, 2026-11-13): the plan, a Redis database
made and placed, the image service made with its link, port and variables and deployed, and a
second run finding everything there. The calls are the API's routes
as documented.

## A service and its database

Railway's and Render's templates make an app and its database together. roll does it without a
template: the plan reads what the code talks to (`Plan.uses`, from the root's manifests: `pg`,
`psycopg`, `pgx`, `sqlx` with `postgres`, `org.postgresql`, and `redis`, `ioredis`, `sidekiq`,
`go-redis`… in package.json, requirements.txt, pyproject.toml, Pipfile, Gemfile, go.mod,
Cargo.toml/Cargo.lock, composer.json and Maven/Gradle files; only `dependencies` in
package.json, since a dev dependency is usually a test's), with the reason in the notes. New
service ticks the project's one database of that kind, or a New Postgres/New Redis chip when the
project has none (with two or more, the user picks), and stops guessing once the user touches a
chip. On Deploy the sheet makes each new database in its product (`roll db`, `roll redis`),
places it beside the service on the canvas, and creates the service linked to it, so
`DATABASE_URL`/`REDIS_URL` are in the first deploy. A database made before a failed create stays
in the project, and a retry links it rather than making another. The first `roll up` in a
directory prints the same finding with the two commands that do it.

## Deploy hooks

An environment can have a deploy hook (Render's): a secret URL that deploys it with no API
token, for CI or a registry's webhook (`POST …/hook`, "Deploy hook" in the environment panel,
`roll apps hook`). Only the token's hash is kept, and a new one replaces the old. A POST to it
deploys the service's own source as a manual deploy would (actor `deploy hook`, and an
`audit.hook.deployed` entry with the caller's address, since the API's gate never sees it); for an image
service, `?image=` may name another tag or digest of the same repository
(`docker.io/library/nginx` stays nginx), never another image, so a leaked URL can only redeploy
what the service already runs. Any token that isn't the live hook's is the same 404. The token
is `rlk_`, 34 base32 characters and a CRC-32, as roll's other secrets, so secret scanning can
find a leaked URL. Making or removing one is admin in roll MCP's catalogue: it's a standing
credential. rolld's gate lets `/v1/apps/hooks` through to the hook's own check (its `RULES`);
access lines log the route pattern, never the token; it runs on the service's owner.

## Static sites

A build whose plan serves files (busybox httpd on `/site`: a plain `index.html`, Vite, CRA,
Astro without an adapter…) runs no instance (Render's and Vercel's static sites). After the
build, the runtime exports the image's `/site` as a tar (`Runtime::export_files`: a `docker run
--network none --entrypoint tar`, its exit checked) once per image, kept on the node's disk
under `sites/` and in object storage for the node that takes the service over, keyed by the
image's digest (or a built image's tag hash), so a rollback or a promotion reuses it. The owner's
proxy answers from the tar directly: an index of its regular files (name → offset and length)
read once per node, byte ranges read per request. Nothing is extracted, so a tenant's archive
can't place a symlink or a `..` path anywhere: anything that isn't a regular file with plain
path components is left out of the index. GET and HEAD; a file, a directory's `index.html`, a
single-page app's index for a page path, or the site's `404.html` with a 404; ETags (304s),
`no-cache` on HTML and five minutes on the rest, `nosniff`. Requests and bytes are metered and
access-logged as any (instance `site`), and nothing else is: no CPU or memory, no cold start.
A gradual rollout splits by bucket as usual, and a deployment's own URL serves its files too.
A start command set in the service, a site over 512 MiB or 100,000 files, or a runtime that
can't export (the fleet's CodeBuild builder, until its first run) means a container serves it,
as before. A service's sites are deleted with its images.


A site's own `_redirects` and `_headers` (Netlify's and Cloudflare Pages' files and syntax) are
read when its files load, never served themselves (`site_rules.rs`). `_redirects`: `/from /to
[status]`, `*` and `:splat`, `:name` segments, 301 by default; 301–308 redirect (the query kept
unless `to` has one; another site's URL allowed), 200 serves another of its files where none is
at the path (`/* /index.html 200` never hides a real asset), 404 answers one with a 404; the
first match wins, at most 2,000 rules. `_headers`: a path line, then indented `Name: value`
lines; every matching block adds its headers over roll's own (`Cache-Control` for one), at most
100 blocks, the response's framing (`Content-Length`, `Transfer-Encoding`, hop-by-hop) roll's.
Each file is at most 64 KiB; a line that doesn't parse is skipped, as theirs are.
## Maintenance and draining

An environment in maintenance (Render's maintenance mode; Settings' "Maintenance", `roll apps
env --maintenance [NOTE]`, `--serve` to end it) answers its public names with roll's 503 page,
the note as text (never markup), `Retry-After: 30`, and wakes nothing, so a sleeping service
costs nothing meanwhile. A deployment's own URL still answers, to try a fix first; the private
network is untouched. The setting is kept with the environment's live state on its owner, read
from the store once, so a request pays nothing for it.

A web service's `run.timeout_s` (1–3600, Cloud Run's request timeout; `roll apps set
--timeout`) is how long a request has until the app answers: past it the visitor gets a 504 and
the access log a 504. It runs to the answer's head, so a WebSocket or a stream that has begun is
the app's to keep; unset, there's no limit, as before.

`run.drain_s` (1–300, Render's `maxShutdownDelaySeconds`; `roll apps set --drain`) is how long
an instance being replaced (a deploy, a promoted or aborted canary) or scaled in gets: its
requests in flight finish, then SIGTERM, and SIGKILL when the time is up (a second after SIGTERM
at least). Unset, 30 s. Each instance has its own window, so a slow one doesn't use up
another's.

## Private services

`run.public: false` (Settings' "Reachable", `roll apps set --public false`) takes a web service
off the internet, as Render's private services: its `up.roll.site` names, its deployments' own
hostnames and its custom domains are 404s like an address nobody has, and `Environment.url` is
null. It's read from the service's settings on each request, so it takes effect at once, not
with the next deploy. The project's services still reach it at `<svc>.<env>.internal` on their
network, which wakes it ("A private caller wakes a sleeping service"). Its previews have no
public URL either.

## Scaling on CPU

A web service scales on its requests (`concurrency` per instance); with `run.cpu_target` (10–100,
Render's target CPU, `roll apps set --cpu-target`) it scales on CPU too: the owner keeps a minute
of its awake instances' measured CPU against what they could use (`run.cpu` each), and while the
average is over the target another instance starts, one at a time, with a fresh minute to judge
it by (the need it had is kept meanwhile, so the new one isn't taken straight back in). Fewer
stay once the instances left would be under the target, through the same scale-in as requests
(`scale_down_after`). Only measured CPU counts: Docker's cgroup, not Fargate's tasks.


Session affinity (`run.affinity`, Cloud Run's; Settings' "Session affinity", `roll apps set
--affinity true`) keeps each visitor on one instance while it's up and has room: the proxy sets a
`roll_instance` cookie (a digest of the instance's name, taken out of what the app sees), and a
request carrying it goes to that instance before any other. When it's gone or full another
answers and the cookie moves to it. For apps that keep sessions in memory or fan WebSockets out
by instance; the rollout bucket still decides the deployment first.
## Allowed networks

An environment's allow list (Render's `ipAllowList`; Settings' "Allowed networks", `roll apps env
--allow-ip CIDR …`, `--allow-any`) lets only its networks in through the public names, its
deployments' own URLs included. It's the first thing the proxy checks, before access, maintenance
or a wake, on the address the fleet vouches for (`roll_cluster::client_addr`: a forward's signed
client, the edge's, else the peer), an IPv4 client over IPv6 counted as IPv4; a request with no
address is refused. At most 100 networks, kept canonical (`10.1.2.3/8` is `10.0.0.0/8`). It's
read with maintenance into the environment's live state on its owner, so a request reads no row.
The project's private network doesn't go through the proxy and isn't affected.

A rate limit (Vercel's firewall rate limits; Settings' "Rate limit", `roll apps env --rate-limit
N[:BURST]`, `--no-rate-limit`) gives each client address `burst` requests at once (default: a
minute's worth) and then `per_minute`, a token bucket; more get a 429 with `Retry-After` (when
the next token comes), right after the allow list and before anything wakes. The buckets are the
owner's, in memory: at most 100,000 clients, the full ones forgotten first (they're the same as
none), and all of them when the limit changes.

## Reaching a service from your machine

`roll apps proxy [-s service] [--local-port N]` (Fly's `fly proxy`) forwards `127.0.0.1:N` (default:
the service's port) to a web service's port through roll: each connection is a tunnel,
`GET …/environments/{env}/tunnel`, a WebSocket whose binary messages are the connection's bytes.
The owner wakes an instance and holds it (a lease, as a request does) while the tunnel is open,
connects only to the service's own port, and puts `tunnel.opened` in the activity. It's how a
private service's admin page or internal API is reached without making it public; it goes past
the allow list, access token and maintenance, so it's `admin`, as a shell is. At most 20 per
environment. Checked live against rolld on Docker: a private busybox `httpd`, asleep, answered
the first `curl` through the proxy (the wake), then in 6 ms.

## Workers that follow their work

A worker with `run.scale` (`{ stream, group, per_instance }`) runs one instance per
`per_instance` messages its roll stream consumer group is behind, between `min_instances` and
`max_instances`, as no general platform can: roll owns the stream (Render scales workers on CPU
and memory, KEDA on queue length outside the platform). Every ten seconds the service's owner
reads the group's lag (its committed offset on each partition against the partition's high
watermark, as the broker's `GET …/groups/{g}` adds it up). A stream is named by its id (`st_…`),
never a name, and a group by Kafka's characters. It's read only for a stream on the service's
own project, and only on a project the operator owns: anyone can put any reference on their own
canvas and streams don't know their owners yet, so a person's project doesn't follow a stream
(as roll secrets refuses links there), and roll MCP's gate wants read on the stream to set it.
A deploy starts a scaled-out worker at the instances it wants, not back at its floor; a note
about scaling is logged when it changes, not every ten seconds; and a web service's `scale` is
dropped (it scales on its requests); the reaper starts instances at once and
stops one (the newest, given the drain to finish its work) only after fewer were wanted for
`scale_down_after`. rolld reads the lag from its own API (`GET /v1/streams/{s}/groups/{g}`, the
operator's token), which on a fleet forwards to the stream's owner, the only node whose numbers
are right (checked against a live rolld: 30 records, a group committed at 5, lag 25). Queues
follow once a queue is a workspace resource (its project is what keeps it a tenant's). Settings
picks the stream from the project's own, with the group and messages per instance (`roll apps
set --scale <stream>:<group>[:<per instance>]`), and the environment shows the backlog last read
and the instances it wants (`Environment.scaling`).


A worker's restart policy (Railway's; Settings' "Restart", `roll apps set --restart
always|on_failure|never [--restart-tries N]`) says what happens when it stops: `always` (the
default) starts it again with the backoff, `on_failure` only after a nonzero exit or out of
memory, up to `restart_tries` in a row (10), and `never` leaves it. A clean exit under the last
two is the work done, not a crash (no crash loop). What the policy leaves stopped counts toward
the worker's instances, so nothing takes its place, until a deploy or a restart starts fresh
ones.
## HTTP/2 and gRPC

`run.h2c` says the app speaks HTTP/2 without TLS, as gRPC servers do (Cloud Run's
`--use-http2`, Fly's `h2_backend`): the proxy forwards to it over an HTTP/2-only client (prior
knowledge), keeps `te: trailers` (gRPC refuses requests without it), and its health checks are
h2c too. Every response's frames pass as they came, so trailers (gRPC's `grpc-status`) reach the
client; this was true of no response before, since the proxy only copied data frames. A
WebSocket upgrade stays HTTP/1.1. Clients reach it over HTTP/2 on rolld's own apps listener (h2c
or HTTP/1.1); on the fleet the edge's HTTPS listener offers only `http/1.1` today, so gRPC from the
internet waits for it to offer `h2` (in `## Next`).

## A deployment's own URL

Every built deployment of a web service without a volume answers on its own hostname,
`<id>--<slug>` (the id without `dep_`), whatever is live (Vercel's deployment URLs, Cloud Run's
revision tags): the same environment's access rules, one instance at most (made from that
deployment's image, variables and settings), gone a minute after its last request, metered with
the environment. The active one's URL is just its instances. With `no_traffic` (`roll deploy
--no-traffic`), a deploy builds, pulls and health-checks, then stops at `ready` without
switching anything, so it can be tried at its URL; "Put live" (a rollback to it) runs its image
again, no build, through a gradual rollout when the service has one. A no-traffic deploy neither
supersedes a queued deploy nor is superseded by one, decides on the run it built (a roll.toml
that makes it a worker fails it), and doesn't run `pre_deploy` (a try-out never migrates the
shared database; putting it live does). No environment's slug contains `--` (slugs collapse
runs of `-`), so no hostname a tenant can get looks like another's deployment. A deployment's
URL answers with that deployment or not at all, never quietly with the live one; its instance is
metered even when nothing's live, and isn't counted in the environment's scaling or a canary's
judgement. A service with a volume has neither: one instance writes it.

## Following a tag

An image service's environment can follow its tag (`follow_tag`; "Follow the tag" in the
environment panel; `roll apps env --follow-tag true`), as Railway's image auto updates do:
every five minutes the owner asks the registry what the tag points at (a manifest `HEAD`
through `Runtime::remote_digest`, no pull, with the service's registry login when it's that
registry's, after the same public-address check a deploy makes) and compares it with the digest
its live deployment pinned. A new one deploys (actor `tag watch`, "nginx:1.27 moved to
…"), through everything a deploy does: pulled and pinned again, health-checked, a gradual
rollout when the service has one. Each environment remembers the digest it last acted on, and
only a move past it deploys: a rollback while following isn't undone, and a digest whose deploy
failed isn't tried again every five minutes. A deploy already on its way is left alone, and a registry
that doesn't answer is a line in the environment's log. Off by default: a deployment runs the
digest it pinned until someone deploys, so a push to a tag never changes production by
itself unless asked.

## Volume backups

A volume is backed up to object storage as a tar (`apps/volumes/<service>/<env>/<unix ms>.tar`),
on demand (`POST …/volume/backups`, "Back up now" in the environment panel, `roll apps backups
--now`) and once a day by its owner, the newest seven kept, as
Railway's volume backups and Render's and Fly's snapshots. A throwaway busybox container with no
network mounts the volume read-only and streams `tar c` into `Objects::upload`, so nothing holds
the volume in memory; it's the disk at that moment, so an app mid-write can leave a file half
written, as any snapshot of a running disk. The container's exit is checked at the end of the
stream, so a backup that stopped short (a daemon error, an image it couldn't pull) fails instead
of being stored, and never rotates a good one out. A restore (`…/backups/{id}/restore`)
downloads the backup whole and checks it against its listed size before touching anything,
then holds the environment asleep under its wake lock (requests get a 503 for that moment;
rollouts and cron runs on the volume wait or refuse), extracts the tar into a staging directory
on the volume, and only once it extracted whole swaps the old files for it; a broken tar leaves
the volume as it was. It runs on its own task, so a client that goes away can't leave it half
done or the environment held. The next request wakes it. The hourly backups run on their own
task too, never on the reaper's loop. Both are in the audit
trail. Without object storage on the node, backups answer 409. (Fargate has no volumes yet.)

## Log drains

A project can send its services' runtime output to up to five HTTPS endpoints
(`POST /v1/apps/projects/{project}/drains`, Settings → Log drains), as Render's log streams and
Fly's and Heroku's drains do: NDJSON batches of at most 500 lines or a second, each line its
time, service, environment, instance, deployment, stream and text, signed as webhooks are. Each
node sends what its own instances write (a bounded tap on its log hub), so nothing crosses
nodes and no line waits on another node. It's best effort, as drains are: a batch is tried three
times and then dropped, and a drain that can't keep up loses its oldest lines (counted in the
node's log) rather than slowing a writer or holding memory. Endpoints are tenant-chosen URLs:
the public-only client, no redirects, private addresses refused unless
`ROLL_APPS_ALLOW_PRIVATE`.

## Spend limits

A project can have a monthly alert and a hard limit (`PUT /v1/apps/projects/{project}/limits`,
Settings → Spend limits, `roll apps limits --alert 20 --hard 50`), in US dollars at the prices
in "Observability and cost", over its services' metered use this calendar month (UTC), as Railway's usage limits
work. One node (`apps:spend`) checks every 30 s: past the alert it writes one
`spend.alert` event for the month; past the hard limit one `spend.limit` and marks the project
capped. Each mark is a compare-and-set in the event's own transaction, so a handover never sends
two. Every node reads the capped services every 30 s and caps their environments here: running
instances stop, no floor is kept, a wake answers 402 ("Paused"), and deploys, jobs and cron runs
are refused. Changing the limits clears the month's marks; the next pass decides them again, so
raising the limit lifts the cap within 30 s. Notification targets get both events by default.
Spend counts the services a project has now (a deleted service's month drops out).

## Measured

- A web instance's network lives in a sandbox made with it (`Docker` with `netns`, the default;
  `ROLL_APPS_NETNS=0` turns it off): busybox asleep, holding the network namespace, its aliases
  on the project network and its published port, with no capabilities, 16 MiB, half a vCPU, 8
  pids and a read-only root (at a twentieth of a vCPU, its own start was throttled to 800 ms
  instead of 170, on the path of every new instance). The app joins that namespace (`NetworkMode: container:<sandbox>`), so
  a wake starts only the app, with no network set up on its path (the same idea as Kubernetes'
  pause container). Snapshot restore (CRIU) would go further, but this host has no CRIU and
  Docker's checkpointing is experimental. Measured on real Docker
  (`a_cold_start_skips_network_setup_with_a_sandbox`: the same image, five alternating pairs):
  median 151 ms against 238 ms, and in a second run 171 against 375, faster in five of five each
  time; the standard cold start went from 258 to 175 ms median and Node's from 313 to 199 ms.
  The sandbox (and its waker) is removed with its instance, and a node taking a service over
  keeps a known instance's sandbox and waker.

On ai-server (36 cores, Docker 29, overlayfs, load around 25–30 from other work), 2026-11-12.
`crates/apps-control/tests/docker.rs` (`pnpm test:apps --docker`) prints the first three; the Node
row is `roll up` of a two-file Node app against a real rolld, timed with curl over Tailscale.

| path | what | median |
| --- | --- | --- |
| warm | a request to a running instance through the proxy | 1 ms |
| resume from suspended | unpause → first byte | 13 ms |
| cold start from stopped (busybox static site, 0.25 CPU) | start → first byte | 260 ms |
| cold start from stopped (Node 22, `node server.js` run directly) | the same, Node booting | 360 ms (static in the same run: 364 ms) |
| first deploy, static site | upload → build (base image pulled) → health check → live | 12.5 s |
| first deploy, Node | `roll up` → build (`node:22-slim` pulled) → health check → live | 11.1 s |

- A wake talks to dockerd's socket (`runtime/engine.rs`), not the `docker` CLI. Four alternating
  pairs of the Docker test at load 35–38, CLI against socket, medians per run: cold start
  296/293/299/325 ms against 270/263/270/285 ms (all four pairs won, about 9%); resume
  48/41/45/45 ms against 14/13/16/13 ms (3.3×). What's left of a cold start is dockerd starting
  the container and the app's own boot.
- **Startup CPU boost** (Cloud Run's): an instance starts with 4× its CPU (at least 1, at most
  4 cores) and is held to its own limit once it answers (`Runtime::settle`; the limit is the
  container's `sh.roll.apps.cpu` label, so a new process of rolld still applies it); a stop
  gives the next start the boost again, off the wake's path. Measured on the static site at
  0.25 CPU, 5 alternating pairs: medians 291/326/294/274/354 ms without it against
  243/260/267/268/249 ms with it (all five won, 294 → 260 ms overall, and a far tighter spread);
  the tiny Node server at 0.5 CPU: 329/324/328 against 315/320/319 ms (three of three, about
  3%). The Docker test checks that a running instance is back at its 0.25 CPU. What it uses is
  metered from the cgroup, so the boost is billed as used.
- A start script that is just `node …` runs without npm in front of it (apps-build,
  `Pm::start`). Before that and the socket, a Node cold start was 665 ms against 443 ms for the
  static site in the same conditions; now the two are the same within noise (360 and 364 ms,
  `generated_node_python_and_go_images_build_and_answer` and the static test, one run). That
  test also builds the generated Node, Python and Go Dockerfiles for real.
- A static site from its files (`a_static_sites_files_answer_without_a_container`, real Docker):
  the first request 2 ms (it reads the site's index), warm 0 ms, against a 258 ms median cold
  start for the same site in a container; no container exists for it at all.
- Idle cost: a service asleep (stopped) meters nothing: no CPU-seconds, no GB-seconds, $0.00.
  The only idle cost is the image on disk (roll's, not the user's). Suspended instances keep their
  memory on the host (roll's cost, not billed) for `sleep_after` (10 minutes by default).
- The service page follows `GET …/changes` (server-sent ticks from the owner: a deploy's steps
  and end, every audited change, and its instances' states, compared in memory each second) and
  reads the service again 250 ms after a burst of them; while that stream is up it polls once a
  minute as a safety net instead of every 3 s during a deploy and every 15 s otherwise, and
  nothing at all while the tab is hidden.
- An environment's read (the dashboard's every few seconds) worked out `needs_deploy` each
  time: every variable decrypted and every link resolved, then the fingerprint. It's cached per
  environment, keyed by the active deployment and a generation every settings, variables, link
  or environment change bumps (after its write, so a read racing a change never caches the old
  answer), and good for a minute so a linked database's own change still shows. With 40
  variables and one link (debug build, the fake link resolver, which answers at once), 300 reads
  took 6.06 ms each cold and 2.10 ms cached, median of five alternating pairs, 61–71% faster in
  all five. Against a real pg-control, resolving a link is a network call, so the saving is
  larger.

## Builds in detail (apps-build)

- Node yields to another language: a tree with a Gemfile, composer.json, requirements.txt,
  pyproject.toml, manage.py, go.mod, Cargo.toml, pom.xml or build.gradle isn't a Node app even
  with a package.json; a package.json with a `build` script there adds a Node `assets` stage.
- Manifests are copied before the source, so dependency installs are a cached layer, except when
  the install may read other files (workspaces, install scripts, `patches/`, `-r`/`-e` lines,
  gemspecs, Composer path repositories) or `install` is overridden.
- Framework entries run directly (Nuxt `node .output/server/index.mjs`, Nest `node dist/main`,
  SvelteKit adapter-node `node build`, Astro's node adapter); Next and Remix use the start script.
  Static builds (Vite, CRA, Gatsby, Astro without an adapter, adapter-static) are provider
  `static` on busybox httpd; one with a client-side router is a single-page app ("Single-page
  apps").
- Go runs on distroless static (no shell); Rust builds on `rust:<v>-slim-bookworm` so the binary
  runs on `debian:bookworm-slim`; Java runs the built jar on a JRE image; Python installs into
  `/opt/venv`; PHP serves `php -S` with `PHP_CLI_SERVER_WORKERS=4`.
- Every generated start command takes `$PORT` (default 8080); only a tree's own Dockerfile sets
  another port, through its last `EXPOSE`.

## Not done yet

- The Fargate runtime and the CodeBuild builder are built and tested against fake AWS APIs
  (`crates/apps-control/tests/fargate.rs`: a web service built, health-checked as a task, woken
  and put to sleep; a failed build; a cron run's exit code; no variable in any `RunTask` or
  `StartBuild` request), not against AWS. The stack (`deploy/cloud/aws/roll-cloud.yaml`) doesn't
  have them yet: the CodeBuild project (privileged; its role may push to `apps/*`, read
  `<prefix>builds/*` and the `roll-apps/builds/*` secrets), the execution role (pull `apps/*`,
  read `<prefix>env/*`, write the log group), the fleet role's rights (`RunTask` of `roll-apps-*` families, register task
  definitions, pass the execution role, `StartBuild`/`StopBuild`/`BatchGetBuilds` on the project,
  ECR `CreateRepository`/`BatchDeleteImage` under `apps/`, Secrets Manager under
  `roll-apps/builds/`), a bucket lifecycle rule expiring `<prefix>env/` and `<prefix>builds/`
  after a day, and the `ROLL_APPS_*` node settings. A private image pulls with the service's
  login as a Secrets Manager secret `roll-apps/registry/<service>` named in the task definition
  (repository credentials; tested against the fake AWS): the fleet role needs
  `CreateSecret`/`PutSecretValue` there and the execution role `GetSecretValue`; the secret
  isn't deleted with its service yet. Still missing on the fleet: private networking (Cloud
  Map), volumes (EFS; a service with a volume is refused), shells (ECS Exec), and multi-line
  variable values.
- The edge routes `*.up.<site domain>` and unknown hosts (custom domains) to roll apps.
- Custom domains' certificates are built and tested against Pebble (HTTP-01) and a fake
  Cloudflare API (Cloudflare for SaaS), not against Let's Encrypt or Cloudflare. For
  `ROLL_APPS_TLS=cloudflare` the site zone still needs Cloudflare for SaaS enabled, `up.roll.site`
  proxied and set as the fallback origin (`PUT /zones/{zone}/custom_hostnames/fallback_origin`),
  the token's SSL and Certificates edit on the zone, and an SSL mode for custom hostnames that
  accepts the fallback origin's certificate for the custom hostname's server name (Full, or Full
  (strict) with an Origin Rule overriding SNI to `up.roll.site`). Custom hostnames aren't held to
  authenticated origin pulls, and their `CF-Connecting-IP` isn't believed yet: the edge classes
  them as unknown hosts. Domains' state comes from the minute loop (while `pending`) and Verify;
  Cloudflare's webhooks for custom hostnames aren't used, and a custom hostname whose validation
  timed out isn't re-validated (removing and adding the domain again starts over).
- An instance's writable layer and volume have no disk quota (Docker's `--storage-opt size`
  needs overlay2 on XFS with project quotas, which a node may not have), and from a container
  `host.docker.internal` reaches every listener on the host, not just the linked resources'
  ports; both are the host's set-up (the fleet runs tasks, not containers on a node). roll-fetch
  writes a bundle's env file 0644 (what reads it in previews' containers decides whether it can
  be 0600). Builds' secrets a stopped node left in Secrets Manager are swept at start (two days
  old); the bucket's `builds/` and `env/` objects wait for the stack's lifecycle rule.
- Links: Postgres and Redis are wired; buckets, streams, queues and NoSQL answer "not wired yet".
- roll secrets isn't on main yet: `NoSecrets` stands in, and service variables are the env store.
- Private networking is a Docker network per project and environment; it isn't reachable from
  the host's own roll databases on hosts whose firewall drops container-to-host traffic.

## With the other platform branches

Reviewed on 2026-11-13 against main 13d4a1b7b (#18 accounts, #19 MCP), feature/roll-mcp-next
b691d1029 (#24), feature/roll-secrets 55a618718, feature/roll-previews 8c7f6ecdf and
feature/roll-search 7f92c2f28, the held apps work being wip/roll-apps-next (pairwise
`git merge-tree` too). Ours are done or in `## Next`; theirs have been told.

- **roll-mcp** (#19 merged into main at 11:15; #24, feature/roll-mcp-next, follows): every route
  in `api::router` resolves to a catalogue row, and http-api.md's
  classes follow the catalogue (restore is `delete`, a domain's PATCH `deploy*`, a job's logs
  `reveal`, hooks `admin`, wake `write`, all of a shell session `admin`). Ours on merge:
  - Done in the stacking merge (1f0caf874, with accounts #18): `/v1/apps/hooks` in rolld's
    `UNSCOPED`; `rlk_` (not `rlh_`, main's automations' prefix) in `SECRET_PREFIXES` and the
    product mapping, and `Access::on_leaked("rlk_")` revoking through
    `Apps::revoke_leaked_hook`; the `"apps"` environment resolver (ephemeral is "preview");
    `api::actor` naming the token's grant, then a signed-in admin, then the operator. The GitHub
    partner pattern is `rlk_[a-z2-7]{40}`.
  - Still open: `put_link` (and create's `links`) checking `Grant::allows(<resource>, reveal)`
    itself (the catalogue's `.lends()` does it at the gate today).
  - `net.rs` and `model::private_ip` move onto mcp's `roll-net`.
  - #24: `apps_deploy` takes `no_traffic`; `apps_set_environment` covers `PUT
    …/environments/{env}` but `access` and `feedback`; `run.scale` needs read on its stream at
    the gate (a row's lends name each resource's own action), and `apps_update_service` says
    so.
- **roll-previews** (`RollApps`): new Environment fields are ignored safely, `ready` falls into
  its success branch (it never sends `no_traffic`), and apps no longer runs canaries in
  ephemeral environments. Theirs: `unset` the variables it set before and no longer wants (an
  override removed today stays in the `pv-` environment); once secrets is wired, PATCH only the
  link redirects, not `t.vars` (a second copy of project secrets in apps); `catalog()`'s
  memory ignores `[app.previews.run]` and per-environment `run`; `--env K=V` on argv in
  `code-preview/src/docker.rs` (#20's change). Previews sends `feedback: true` (its own rule:
  the setting, a change, an access token) once apps' follow-up is in its tree: the held #21
  answers 400 to an unknown field. One copy of `feedback.js` and its tagger after the merge.
  Once #26 and #28 are both on main, rolld calls `apps.set_feedback_notes(…)` with an adapter
  over previews' `Engine::notes_for(env, path)` (`p.engine` in `rolld/src/previews/mod.rs`),
  mapping its `Note` onto apps' (`path, selector, note, by, at`, ms).
- **roll-secrets** (see "Secrets and environments"). The wiring is #31
  (`feature/roll-secrets-apps`, after #20 and #26): rolld's `secrets` becomes secrets' `ForApps`
  (resolve with `linked` from `Values.linked`, nothing where secrets has no environment, the
  project's trust of the service; `may_reveal` by `Secrets::caller_of(operator, user, grant)`
  and `allows(Reveal, Scope::at(project, env, "/"))`). Ours on wiring:
  - apps never ensures a secrets environment: an apps environment roll secrets doesn't have
    reads as no secrets (previews ensures its `pv-` ones with their base and trust; ensuring
    again with other values would be a 409 `mismatch`), and a person adds one in roll secrets
    when they want secrets there.
  - Done on apps' side: `GET …/variables/values` asks `SecretSource::may_reveal(who, project,
    env)` (false by default) when any value comes from roll secrets, and answers 403 naming them
    if not; secrets' `ForApps` answers it by its own rules (`Caller::allows(Reveal, …)`, protected
    environments, a token's scope).
  - Seal with secrets' `Keyring` (`ROLL_SECRET_KEY_PREVIOUS`, `ROLL_SECRET_KEY_KMS`), not
    `ROLL_SECRET_KEY` alone, so a key rotation by secrets' guide keeps apps' sealed variables,
    registry logins and target URLs readable, and KMS works on a fleet.
  - `SecretSource` gains `digest`, on `Secrets::digest(project, env, path, caller)` (what `roll
    run --watch` polls: keys, versions, fingerprints and links; it decrypts nothing and isn't
    audited), for `needs_deploy`'s minute-long cache. A live link's resource rotating on its own
    doesn't move it, as with `roll run --watch`.
  - A value whose source is a secret needs `Caller::allows(Reveal, Scope::at(project, env,
    path))` on top of apps' `reveal` (secrets confirmed: otherwise apps' reveal gets around
    protected environments). `Keyring::new(current, previous)`, the same key material.
  - `rolld/src/secrets/syncs.rs`'s `PublicOnly`: #19 is on main, so #20 takes `roll_net`
    (wip/secrets-on-mcp's resolution, which still needs main's #18).
  - Once people manage secrets on their own projects, a canvas stops proving ownership: a
    person-owned project counts only for services that person owns (apps' `SecretSource`, and
    already scaling on a stream).
- **roll-search** (#22 on main; #29 follows): its conflicts with apps are additions side by side
  (`Product`/`Kind` arrays grow). A link to a search cluster gets its own API key through
  apps' `Resources::link`/`unlink` (#26): whichever of #29 and #26 reaches main second
  adds `LinkKind::Search` (`ALL`, `as_str`, `parse`) and, in rolld's `RollResources`, `link` as
  `search_control::Control::mint_link(ref, service)` into `Minted { vars, kept }` and `unlink`
  as `revoke_link(ref, kept)`. The key is data-only (no security privileges), its variables
  `ELASTICSEARCH_URL` and `ELASTICSEARCH_API_KEY` (kept `key:<id>`); an OpenSearch cluster's is an
  internal user with a data-only role, `OPENSEARCH_URL` (kept `osuser:<name>`; revoking removes
  the user and its role). The glue injects whatever variables `mint_link` returns.
- **Conflicts with apps**: previews merges cleanly. With main (and #24), six files from apps
  merging feature/accounts rather than main's squash of #18 (`AGENTS.md`, `rolld/src/{auth,
  cluster,main}.rs`, `tests/auth.rs`, `docs/workspace.md`): take apps' side, keeping one early
  `workspace::open`, and one `/v1/apps/hooks` row in `tests/auth.rs`; merging main into
  feature/roll-apps clears them. With search: additions side by side (`[Product; 10]`,
  `[Kind; 8]`, the dashboard's kind maps). With secrets: eleven files, all trivial but
  roll-workspace's `delete` (keep main's `delete(scope, id)` with `on_delete`);
  `routeTree.gen.ts` is regenerated, not merged.
- **Every product sharing the system database** reserves its own share of
  `ROLL_SYSTEM_DB_CONNECTIONS`: apps has `DbBudget::apps`, previews its own share; mcp still
  borrows the workspace's. `DbBudget`'s test (`split(Some(12))`, at most 12) can't pass once
  apps, search, secrets and previews are combined (seven weighted pools, four fixed and two
  reserved make 13 at least): move it to 16 and count the fixed pools as reserved.
- `scripts/test-apps.sh` runs its own `cargo test -p` calls, not `scripts/lib/gate.sh`'s shared
  package set (adding apps there would slow roll code's and agents' gates).
- Merge order: mcp, secrets, search, apps (then the secrets wiring), previews.

## Next

Ranked; the top item not marked *waiting* is what's being worked on. Three reviews on
2026-11-13 (bugs and security in the newest work, the product against Railway, Render, Fly,
Vercel, Cloud Run and Heroku, and integration with the other branches' heads) set this order.

1. *Waiting on queues as a workspace resource.* **Workers that follow a queue's depth**, as they
   follow a stream group's lag now (a queue's project is what keeps another tenant's depth
   unread).
2. **Region**: the region shown on the overview and each environment, defaulting to the project's
   databases', then a choice per environment once the fleet has a second region (Railway's,
   Fly's). *Waiting on the fleet.*
3. **Static outbound IPs**: a NAT gateway with Elastic IPs on the fleet's subnets, listed in
   Settings, without stopping services from sleeping (Railway's and Render's). *Waiting on the
   user's first AWS run.*
4. *Waiting on the edge's owner (roll cloud).* **gRPC from the internet**: roll-edge's HTTPS
   listener offering `h2` (ALPN `h2, http/1.1`; hyper's `http2::Builder` when `h2` is chosen,
   with the same graceful drain) and setting `Host` from `:authority` (HTTP/2 has no `Host`,
   and every product behind the edge routes on it), so Cloudflare's gRPC reaches h2c apps.
   Done here: `run.h2c` to the app, trailers through the proxy. Then TCP services.
5. *Waiting on #20.* **roll secrets as the env store**, as in "Secrets and environments", plus
   service variables at `/services/<name>`, values resolved only when a container is made,
   `dynamic` links refused, and links as per-environment secrets links.
6. *Waiting on the user.* The fleet runtime in the stack: the roles, project, security group and
   settings listed in "Not done yet", a first run against AWS with its cold starts measured (a
   warm window or `min_instances` 1 by default there until a wake is fast), and
   allocation-based metering for Fargate.
7. *Waiting on #20.* Links for buckets, streams, queues and NoSQL as roll secrets links.
8. *Waiting on the edge's owner (roll cloud).* **Wildcard certificates**: DNS-01 on a delegated
   `_acme-challenge.<base>` CNAME into a zone roll writes (`roll_edge::acme`'s DNS solver at
   another name), then `DomainTls::wildcards` true there; the routing and verification are done.
9. Custom domains' TLS on the fleet for real (the zone settings in "Not done yet", authenticated
   origin pulls for custom hostnames, then a first domain through Let's Encrypt and Cloudflare).
10. **The rest of a cold start**: done: the network in a sandbox (258 → 175 ms); next, snapshot
    restore (CRIU, which needs the host's Docker daemon with checkpoints on: the user's call), or
    Firecracker microVM hosts on the fleet.
11. EFS volumes and Cloud Map private DNS on the fleet; TCP services.
